//! The daemon's Nostr inbox subscription.
//!
//! Every user action Mostro reacts to — `TakeSell`, `AddInvoice`, `FiatSent`,
//! `Release`, `Dispute`, … — arrives over a **single** long-lived subscription
//! opened once at startup. That subscription is the node's only ear, so this
//! module gives it an identity: a stable id plus the filter that defines it.
//!
//! The id matters because the pieces that keep the inbox alive have to be able
//! to *name* it. A relay's `CLOSED` frame carries a subscription id and nothing
//! else; recognising one as "our inbox just died" — and re-issuing the REQ
//! under the same id — is only possible if the daemon decided the name instead
//! of letting the SDK generate a fresh random one per call.
//!
//! Note that the subscription is deliberately built with `.limit(0)`: it wants
//! live traffic, never stored history. The event loop discards anything whose
//! `created_at` is older than ten seconds anyway (see `accept_event` in
//! `src/app.rs`), so asking a relay for a backlog would only pay for frames
//! that are rejected on arrival. The same ten-second window is why a
//! re-subscribe cannot recover what was missed: whatever a user sent while the
//! inbox was down is already too old to be accepted by the time it could be
//! replayed. Losing the ear loses those messages for good — hence
//! [`InboxKeeper`], which exists to make the outage as short as possible.
//!
//! # Keeping the ear open
//!
//! Relays end subscriptions on their own initiative, and say so with a
//! `CLOSED` frame. The SDK's reaction is unforgiving: for nearly every reason
//! prefix — and for no prefix at all — it *removes* the subscription outright,
//! and removed subscriptions are never re-REQ'd, not even across a reconnect.
//! A single frame from a single relay therefore ends the daemon's ability to
//! hear anything, permanently and without a word: the SDK logs it at `debug`,
//! which release builds filter out (`RUST_LOG=none,mostro=info`).
//!
//! [`InboxKeeper`] closes that hole. It watches the control-plane traffic the
//! event loop used to discard, recognises a `CLOSED` aimed at the inbox, and
//! re-issues the REQ to the relay that sent it — under per-relay backoff, so a
//! relay that refuses the inbox on principle is retried at a decreasing rate
//! instead of being hammered.
//!
//! Two reason prefixes are the exception. `auth-required` and `rate-limited`
//! only *mark* the subscription, leaving it registered for the SDK to re-send
//! by itself, so the keeper stands down on the REQ and lets it: see
//! [`is_provisional_closure`]. It does not stand down on the *verdict* — the
//! relay's acknowledgement is dropped either way, because a subscription the
//! SDK left registered is not one a relay is answering, and the SDK does not
//! always get around to re-sending it.
//!
//! Not every way of losing the ear announces itself with a frame, though: the
//! notification channel silently drops messages when the consumer falls
//! behind, a REQ can fail to go out, a relay can be added after startup.
//! [`check_inbox_health`] is the backstop — it asks each connected relay
//! whether it is still serving the subscription, re-subscribes the ones that
//! are not, and records the verdict in [`InboxHealth`] so the rest of the
//! daemon can tell whether Mostro is currently able to hear anything at all.
//!
//! The health record itself — the outage log the scheduler reads, and the
//! per-relay pacing both recovery paths draw on — is in [`health`].

use std::sync::Arc;

use nostr_sdk::prelude::*;
use tracing::{debug, error, info, warn};

mod health;

use health::now_secs;
pub use health::{InboxHealth, InboxStatus, InstallError};

/// Subscription id used for the daemon inbox.
///
/// Fixed rather than the SDK's per-call random id, so a `CLOSED` frame can be
/// attributed to the inbox and the REQ re-issued under the same name. It is
/// visible to every relay, which costs nothing in privacy: the filter's `#p`
/// tag already names this node.
const INBOX_SUBSCRIPTION_ID: &str = "mostro-inbox";

/// The daemon's inbox: the subscription every trade message arrives on.
#[derive(Debug, Clone)]
pub struct InboxSubscription {
    id: SubscriptionId,
    filter: Filter,
}

impl InboxSubscription {
    /// Build the inbox subscription for `mostro_pubkey` on the configured
    /// transport's `event_kind` (1059 for protocol v1 gift wraps, 14 for the
    /// v2 NIP-44 direct messages — see `docs/TRANSPORT_V2_SPEC.md`).
    pub fn new(mostro_pubkey: PublicKey, event_kind: Kind) -> Self {
        Self {
            id: SubscriptionId::new(INBOX_SUBSCRIPTION_ID),
            filter: Filter::new()
                .pubkey(mostro_pubkey)
                .kind(event_kind)
                .limit(0),
        }
    }

    /// The subscription id relays echo back in `EVENT`, `EOSE` and `CLOSED`.
    pub fn id(&self) -> &SubscriptionId {
        &self.id
    }

    /// The filter defining what the inbox listens for.
    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    /// Send the inbox REQ to every connected relay and report the outcome.
    ///
    /// The SDK's `Output` marks each relay individually, and a relay that
    /// refuses the REQ is not an error for the call as a whole — so a node can
    /// come up with a dead ear on some (or every) relay and still look healthy.
    /// That verdict is logged here rather than discarded.
    pub async fn subscribe(&self, client: &Client) -> Result<(), Error> {
        let output = client
            .subscribe(self.filter.clone())
            .with_id(self.id.clone())
            .await?;
        self.report(&output);
        Ok(())
    }

    /// Log which relays took the inbox REQ and which refused it.
    fn report(&self, output: &Output<SubscriptionId>) {
        for (url, err) in output.failed.iter() {
            warn!("Inbox subscription refused by relay {url}: {err}");
        }

        if output.success.is_empty() {
            // Not fatal — relays reconnect, and the watchdog retries — but the
            // node is deaf until one of them accepts, and that must be said out
            // loud. The SDK logs its side at `debug`, which release builds
            // filter out entirely (`RUST_LOG=none,mostro=info`).
            error!(
                "Inbox subscription '{}' was accepted by NO relay: Mostro cannot receive any \
                 trade message until this recovers",
                self.id
            );
        } else {
            info!(
                "Inbox subscription '{}' active on {} relay(s)",
                self.id,
                output.success.len()
            );
        }
    }
}

/// Keeps the inbox subscription alive across relay-initiated closures.
///
/// Lives in the event loop, which is the only consumer of the notification
/// stream. All of its mutable state — acknowledgements and re-subscribe
/// pacing — is in [`InboxHealth`], because [`check_inbox_health`] runs from a
/// different task and has to see and share the very same facts.
pub struct InboxKeeper {
    subscription: InboxSubscription,
    /// Where relay acknowledgements and re-subscribe pacing are recorded. The
    /// event loop is the only place an `EOSE` can be observed, but the
    /// watchdog is what acts on it, so the facts have to be shared rather than
    /// kept here.
    health: Option<Arc<InboxHealth>>,
}

impl InboxKeeper {
    pub fn new(subscription: InboxSubscription) -> Self {
        Self::with_health(subscription, InboxHealth::global())
    }

    pub fn with_health(subscription: InboxSubscription, health: Option<Arc<InboxHealth>>) -> Self {
        Self {
            subscription,
            health,
        }
    }

    /// React to one control-plane frame from `relay_url`.
    ///
    /// Two frames matter for the inbox: `CLOSED`, which means the ear on that
    /// relay is gone and has to be re-opened, and `EOSE`, which is a relay
    /// confirming it accepted the REQ and is the signal used to clear the
    /// backoff. Everything else (`OK`, `NOTICE`, other subscriptions' frames)
    /// is not this module's business.
    ///
    /// Awaiting this inline in the event loop is safe: the re-subscribe
    /// bottoms out in `send_client_msg`, a `try_send` onto the relay's
    /// transport channel with `wait_until_sent: None` (nostr-sdk 0.45.1,
    /// `relay/inner.rs`) — no network round-trip, no blocking send, only
    /// short locks on the subscription map — so a slow or dead relay cannot
    /// stall the loop that every trade message flows through.
    pub async fn on_relay_message(
        &self,
        client: &Client,
        relay_url: &RelayUrl,
        message: &RelayMessage<'_>,
    ) {
        match message {
            RelayMessage::Closed {
                subscription_id,
                message,
            } if subscription_id.as_ref() == self.subscription.id() => {
                if is_provisional_closure(message) {
                    // The REQ is not the keeper's to re-send *on this frame*:
                    // the SDK only *marks* these two prefixes and re-sends it
                    // itself — after the NIP-42 round-trip for
                    // `auth-required`, on the next reconnect for
                    // `rate-limited`. Re-issuing it here, in the microseconds
                    // after the frame arrives, would drop the entry the SDK is
                    // about to re-send, cut across its AUTH, and arm a backoff
                    // against a relay that is behaving exactly as the protocol
                    // says it should.
                    //
                    // The stand-down is scoped to that window and no further.
                    // `check_inbox_health` will re-send the REQ at the next
                    // audit if the relay still has not answered, and that is
                    // deliberate rather than an override of this branch: an
                    // AUTH round-trip completes in well under
                    // `INBOX_WATCHDOG_INTERVAL`, so a relay still
                    // unacknowledged a full interval later is one the SDK's
                    // own recovery did not reach — the `rate-limited` and
                    // rejected-AUTH dead ends below. A relay that *did* answer
                    // is acknowledged and the audit never touches it, so the
                    // backstop costs a redundant REQ only in the case where
                    // standing down permanently would mean silent deafness.
                    //
                    // The *health verdict* is another matter, and must not
                    // stand down with it. `MarkAsClosed` leaves the entry in
                    // the SDK's subscription map, so the relay still reads as
                    // registered; with its earlier `EOSE` also intact the
                    // audit would count it as serving the inbox forever —
                    // including in the two cases the SDK never gets to: a
                    // `rate-limited` closure on a connection that never drops
                    // (`Relay::resubscribe` only runs on reconnect, and there
                    // is no retry timer), and an `auth-required` one whose
                    // AUTH the relay then rejects (the ingester reports
                    // `AuthenticationFailed` and returns without re-sending).
                    // Dropping the credit costs nothing on the happy path —
                    // the replacement REQ is answered and the credit comes
                    // back one audit later at worst — and turns both dead ends
                    // into a re-subscribe under the shared backoff instead of
                    // silent deafness.
                    if let Some(health) = &self.health {
                        health.note_relay_unacknowledged(relay_url);
                    }
                    info!(
                        "Relay {relay_url} closed the Mostro inbox subscription provisionally \
                         (\"{message}\"); recovery is the SDK's or the watchdog's"
                    );
                    return;
                }
                warn!("Relay {relay_url} closed the Mostro inbox subscription: \"{message}\"");
                self.resubscribe(client, relay_url).await;
            }
            RelayMessage::EndOfStoredEvents(subscription_id)
                if subscription_id.as_ref() == self.subscription.id() =>
            {
                // The relay answered the REQ: it is really serving the inbox,
                // which is what the watchdog needs to know, and whatever made
                // it fail before is over, so the next failure deserves a prompt
                // retry again.
                if let Some(health) = &self.health {
                    if health.note_relay_acknowledged(relay_url) {
                        info!("Inbox subscription re-established on relay {relay_url}");
                    }
                }
            }
            _ => {}
        }
    }

    /// Re-issue the inbox REQ to a single relay. Pacing is
    /// [`resubscribe_relay`]'s job, so the watchdog cannot bypass it.
    async fn resubscribe(&self, client: &Client, relay_url: &RelayUrl) {
        let relay = match client.relay(relay_url).await {
            Ok(Some(relay)) => relay,
            Ok(None) => {
                warn!("Relay {relay_url} closed the inbox but is no longer in the pool");
                return;
            }
            Err(e) => {
                warn!("Cannot reach relay {relay_url} to re-subscribe the inbox: {e}");
                return;
            }
        };

        resubscribe_relay(&relay, &self.subscription, self.health.as_deref()).await;
    }
}

/// Whether a `CLOSED` reason means "not now" rather than "not ever".
///
/// These are the two prefixes nostr-sdk 0.45.1 maps to `MarkAsClosed` instead
/// of `Remove` (`relay/inner.rs`, the `RelayMessage::Closed` arm), keeping the
/// subscription registered so it can be re-sent without the keeper's help.
/// Every other reason — and no reason at all — removes it, which is what
/// [`InboxKeeper`] exists to undo.
///
/// `auth-required` is only marked when an authenticator is configured; without
/// one the SDK removes it and no re-REQ follows, but a node in that state has
/// no Nostr keys at all, so the watchdog's pace is the appropriate response.
fn is_provisional_closure(message: &str) -> bool {
    matches!(
        MachineReadablePrefix::parse(message),
        Some(MachineReadablePrefix::AuthRequired) | Some(MachineReadablePrefix::RateLimited)
    )
}

/// Re-send the inbox REQ to one relay, returning whether one actually went out.
///
/// Shared by the event-loop keeper (reacting to a `CLOSED`) and the watchdog
/// (finding an ear that went missing without one), so both recover a relay the
/// same way — and, just as importantly, pace it the same way. Backoff and
/// acknowledgement bookkeeping are part of the operation rather than something
/// callers remember to do:
///
/// - **Pacing.** Both callers draw on one per-relay budget in [`InboxHealth`].
///   The watchdog would otherwise re-send unconditionally on every pass,
///   putting a hard floor of `INBOX_WATCHDOG_INTERVAL` under a ceiling that
///   claims to be `RESUBSCRIBE_MAX_BACKOFF`. Sharing it keeps a transient
///   failure recovering on the very next audit while a relay that refuses the
///   inbox on principle tapers to one REQ every five minutes.
/// - **Acknowledgement.** From the moment a fresh REQ goes out, an earlier
///   `EOSE` says nothing about whether the relay is serving *this* one. A
///   relay that answers, then closes the subscription, then quietly ignores
///   the replacement would otherwise keep its stale credit and read as
///   healthy.
async fn resubscribe_relay(
    relay: &Relay,
    subscription: &InboxSubscription,
    health: Option<&InboxHealth>,
) -> bool {
    if let Some(health) = health {
        if !health.allow_resubscribe(relay.url()) {
            debug!(
                "Skipping inbox re-subscribe on relay {}: backing off",
                relay.url()
            );
            return false;
        }
        health.note_relay_unacknowledged(relay.url());
    }

    // A `CLOSED` does not always remove the subscription: rate-limited and
    // auth-required closures only *mark* it, and a marked subscription is
    // re-REQ'd no earlier than the next reconnect — which may never come on a
    // healthy connection. Dropping the registration first makes the REQ below
    // unconditional, instead of being refused as a duplicate id.
    let _ = relay.unsubscribe(subscription.id()).await;

    match relay
        .subscribe(subscription.filter().clone())
        .with_id(subscription.id().clone())
        .await
    {
        Ok(_) => info!("Re-sent the inbox subscription to relay {}", relay.url()),
        Err(e) => warn!(
            "Failed to re-subscribe the inbox on relay {}: {e}",
            relay.url()
        ),
    }

    true
}

/// Check every read relay, re-subscribing any that is not serving the inbox,
/// and record the verdict in the process-wide [`InboxHealth`].
///
/// The health question is asked of the *subscription*, not of traffic: a node
/// with no trades in flight is legitimately silent, so treating quiet as
/// failure would raise false alarms on an idle instance and, worse, would stop
/// the timeout machinery for no reason.
///
/// A relay counts as serving the inbox only when **it** has said so, by
/// answering the REQ with an `EOSE` the event loop recorded, *on the websocket
/// session it is currently on*. The SDK's own subscription map is not
/// evidence: it records what Mostro sent, so a relay that keeps the connection
/// open and quietly drops the REQ still appears subscribed there — and the
/// daemon would resume timeouts while deaf. Neither is an acknowledgement from
/// an earlier connection: the SDK re-sends the REQ by itself after a reconnect
/// and the relay may ignore that one, which is why the check is
/// [`InboxHealth::has_acknowledged_since`] against the relay's `connected_at`
/// rather than a plain membership test.
///
/// It also means a relay re-subscribed during this audit does not count until
/// it answers, which costs one interval before recovery is declared and keeps
/// the error on the safe side: the timeout clock stays frozen slightly longer
/// than strictly needed rather than restarting too early.
///
/// The audit re-sends to every unacknowledged relay, including one
/// [`InboxKeeper`] stood down on after a provisional `CLOSED`. That is the
/// intended hand-off, not a bypass: the keeper stands down for the instant the
/// frame arrives, so it does not cut across the SDK's own AUTH round-trip, and
/// by the time an audit comes round that round-trip has either produced an
/// `EOSE` — in which case the relay is acknowledged and left alone — or it
/// never will, which is exactly when the REQ has to come from here.
pub async fn check_inbox_health(client: &Client, subscription: &InboxSubscription) -> InboxStatus {
    check_inbox_health_with(client, subscription, InboxHealth::global()).await
}

/// [`check_inbox_health`] against an explicit health record, so tests do not
/// have to install the process-wide one.
async fn check_inbox_health_with(
    client: &Client,
    subscription: &InboxSubscription,
    health: Option<Arc<InboxHealth>>,
) -> InboxStatus {
    let relays = client
        .relays()
        .with_capabilities(RelayCapabilities::READ)
        .await;

    let mut listening = 0usize;
    let mut retried = 0usize;

    for (url, relay) in relays.iter() {
        if !relay.status().is_connected() {
            continue;
        }
        let registered = relay.subscription(subscription.id()).await.is_some();
        let connected_at = relay.stats().connected_at().as_secs() as i64;
        // Without a health record there is nowhere to have stored an
        // acknowledgement, so fall back to registration alone.
        let acknowledged = health
            .as_ref()
            .map(|h| h.has_acknowledged_since(url, connected_at))
            .unwrap_or(true);

        if registered && acknowledged {
            listening += 1;
        } else {
            // Not serving it: a CLOSED the event loop never saw (the
            // notification channel drops frames when it lags), a REQ that
            // failed to go out, a relay re-added after startup — or one that
            // took the REQ and never answered it.
            warn!("Relay {url} is connected but not serving the Mostro inbox; re-subscribing");
            if resubscribe_relay(relay, subscription, health.as_deref()).await {
                retried += 1;
            }
        }
    }

    let status = if listening > 0 {
        InboxStatus::Listening
    } else {
        InboxStatus::Blind
    };

    if let Some(health) = &health {
        let was_blind = health.is_blind();
        health.observe(status, now_secs());

        match (was_blind, status) {
            (false, InboxStatus::Blind) => error!(
                "Mostro inbox is BLIND: no connected relay is serving subscription '{}'. \
                 Trade messages sent now are lost, and order timeouts are on hold until it \
                 recovers",
                subscription.id()
            ),
            (true, InboxStatus::Listening) => info!(
                "Mostro inbox recovered: subscription '{}' is live on {listening} relay(s)",
                subscription.id()
            ),
            (true, InboxStatus::Blind) => {
                warn!("Mostro inbox still blind ({retried} relay(s) retried this round)")
            }
            (false, InboxStatus::Listening) => {
                debug!("Inbox healthy on {listening} relay(s)");
            }
        }
    }

    status
}

#[cfg(test)]
mod tests {
    use super::health::T0;
    use super::*;
    use std::time::Duration;

    fn pubkey() -> PublicKey {
        Keys::generate().public_key()
    }

    /// A keeper backed by its own health record: all of its state — pacing and
    /// acknowledgements alike — now lives there, so tests need the handle too.
    fn keeper() -> (InboxKeeper, Arc<InboxHealth>) {
        let health = Arc::new(InboxHealth::at(T0));
        let keeper = InboxKeeper::with_health(
            InboxSubscription::new(pubkey(), Kind::GiftWrap),
            Some(health.clone()),
        );
        (keeper, health)
    }

    fn relay_url(url: &str) -> RelayUrl {
        RelayUrl::parse(url).expect("valid relay url")
    }

    /// Whether `health` holds any acknowledgement for `url` at all, for the
    /// tests that care about the record rather than the connection it belongs
    /// to.
    fn acked(health: &InboxHealth, url: &RelayUrl) -> bool {
        health.has_acknowledged_since(url, 0)
    }

    #[test]
    fn filter_matches_the_subscription_the_daemon_has_always_used() {
        let key = pubkey();

        for kind in [Kind::GiftWrap, Kind::PrivateDirectMessage] {
            let inbox = InboxSubscription::new(key, kind);
            // The pre-existing `main.rs` filter, spelled out: p-tagged to this
            // node, one transport kind, no stored history.
            let expected = Filter::new().pubkey(key).kind(kind).limit(0);
            assert_eq!(
                inbox.filter(),
                &expected,
                "inbox filter drifted from the daemon's historical subscription"
            );
        }
    }

    #[test]
    fn id_is_stable_across_instances() {
        // A CLOSED frame can only be attributed to the inbox if the id is the
        // same one the REQ went out under — including after a re-subscribe,
        // which builds a fresh `InboxSubscription`.
        let key = pubkey();
        let first = InboxSubscription::new(key, Kind::PrivateDirectMessage);
        let second = InboxSubscription::new(key, Kind::PrivateDirectMessage);

        assert_eq!(first.id(), second.id());
        assert_eq!(first.id().to_string(), INBOX_SUBSCRIPTION_ID);
    }

    #[test]
    fn transport_kind_selects_what_the_inbox_hears() {
        let key = pubkey();

        let v1 = InboxSubscription::new(key, Kind::GiftWrap);
        let v2 = InboxSubscription::new(key, Kind::PrivateDirectMessage);

        assert_ne!(v1.filter(), v2.filter());
        assert_eq!(v1.filter().kinds.as_ref().unwrap().len(), 1);
        assert!(v1
            .filter()
            .kinds
            .as_ref()
            .unwrap()
            .contains(&Kind::GiftWrap));
        assert!(v2
            .filter()
            .kinds
            .as_ref()
            .unwrap()
            .contains(&Kind::PrivateDirectMessage));
    }

    // ───────────────────────── control-plane handling ─────────────────────────

    #[tokio::test]
    async fn eose_for_the_inbox_clears_the_backoff() {
        let client = crate::util::mostro_nostr_client_options(None).build();
        let (keeper, health) = keeper();
        let relay = relay_url("ws://relay.example");

        assert!(health.allow_resubscribe(&relay));
        assert!(health.is_backing_off(&relay));

        let eose = RelayMessage::EndOfStoredEvents(std::borrow::Cow::Owned(
            keeper.subscription.id().clone(),
        ));
        keeper.on_relay_message(&client, &relay, &eose).await;

        assert!(
            !health.is_backing_off(&relay),
            "an accepted REQ must reset the pacing for the next failure"
        );
    }

    #[test]
    fn only_the_two_prefixes_the_sdk_recovers_from_are_provisional() {
        // Mirrors the `RelayMessage::Closed` arm of nostr-sdk 0.45.1: these
        // two map to `MarkAsClosed`, everything else to `Remove`. A future
        // bump that changes the split has to change this list with it.
        assert!(is_provisional_closure(
            "auth-required: we only serve authenticated users"
        ));
        assert!(is_provisional_closure("rate-limited: slow down"));

        for permanent in [
            "blocked: you are banned",
            "restricted: not for you",
            "error: go away",
            "invalid: bad filter",
            "unsupported: no such filter",
            "pow: 24 bits required",
            "duplicate: already have it",
            "",
            "we are closing this one",
        ] {
            assert!(
                !is_provisional_closure(permanent),
                "{permanent:?} removes the subscription, so the keeper has to re-send the REQ"
            );
        }
    }

    #[tokio::test]
    async fn a_provisional_closure_is_left_to_the_sdk() {
        let client = crate::util::mostro_nostr_client_options(None).build();
        let health = Arc::new(InboxHealth::at(T0));
        let subscription = InboxSubscription::new(pubkey(), Kind::GiftWrap);
        let keeper = InboxKeeper::with_health(subscription.clone(), Some(health.clone()));
        let relay = relay_url("ws://relay.example");

        health.note_relay_acknowledged(&relay);

        for reason in ["auth-required: please auth", "rate-limited: slow down"] {
            let closed = RelayMessage::Closed {
                subscription_id: std::borrow::Cow::Owned(subscription.id().clone()),
                message: std::borrow::Cow::Borrowed(reason),
            };
            keeper.on_relay_message(&client, &relay, &closed).await;
        }

        assert!(
            !health.is_backing_off(&relay),
            "a relay the SDK will re-REQ by itself must not be put on the shared backoff"
        );
        assert!(
            !acked(&health, &relay),
            "the relay stopped answering the REQ it acknowledged, so the credit cannot stand"
        );
    }

    /// The SDK does not always get around to re-sending a provisionally closed
    /// subscription: `rate-limited` has no retry timer at all (only the next
    /// reconnect, which never comes on a healthy connection), and an
    /// `auth-required` whose AUTH the relay then rejects ends the ingester's
    /// post-auth path without a `resubscribe()`. `MarkAsClosed` leaves the
    /// entry registered throughout, so registration alone would report a relay
    /// that stopped serving the inbox as healthy for the life of the
    /// connection — the exact silent deafness this module exists to prevent.
    #[tokio::test]
    async fn a_provisional_closure_the_sdk_never_answers_is_caught_by_the_audit() {
        use nostr_sdk::local_relay::LocalRelay;

        let relay = LocalRelay::builder().build();
        relay.run().await.expect("run local relay");
        let url = relay.url().await;

        let subscription = InboxSubscription::new(pubkey(), Kind::GiftWrap);
        let client = crate::util::mostro_nostr_client_options(None).build();
        client.add_relay(url.clone()).await.expect("add_relay");
        client.connect().await;
        subscription.subscribe(&client).await.expect("subscribe");
        tokio::time::sleep(Duration::from_millis(500)).await;

        let health = Arc::new(InboxHealth::at(T0));
        let keeper = InboxKeeper::with_health(subscription.clone(), Some(health.clone()));
        health.note_relay_acknowledged(&url);

        assert_eq!(
            check_inbox_health_with(&client, &subscription, Some(health.clone())).await,
            InboxStatus::Listening,
            "precondition: an answered REQ on a live connection is a healthy inbox"
        );

        let closed = RelayMessage::Closed {
            subscription_id: std::borrow::Cow::Owned(subscription.id().clone()),
            message: std::borrow::Cow::Borrowed("rate-limited: slow down"),
        };
        keeper.on_relay_message(&client, &url, &closed).await;

        let sdk_relay = client
            .relay(&url)
            .await
            .expect("relay lookup")
            .expect("relay in pool");
        assert!(
            sdk_relay.subscription(subscription.id()).await.is_some(),
            "precondition: the entry the SDK leaves registered is what used to vouch for the relay"
        );
        assert_eq!(
            check_inbox_health_with(&client, &subscription, Some(health)).await,
            InboxStatus::Blind,
            "a relay that closed the inbox and has not answered since is not serving it"
        );

        relay.shutdown();
    }

    #[tokio::test]
    async fn a_permanent_closure_is_still_the_keepers_to_answer() {
        use nostr_sdk::local_relay::LocalRelay;

        let relay = LocalRelay::builder().build();
        relay.run().await.expect("run local relay");
        let url = relay.url().await;

        let (keeper, health) = keeper();
        let client = crate::util::mostro_nostr_client_options(None).build();
        client.add_relay(url.clone()).await.expect("add_relay");
        client.connect().await;
        health.note_relay_acknowledged(&url);

        let closed = RelayMessage::Closed {
            subscription_id: std::borrow::Cow::Owned(keeper.subscription.id().clone()),
            message: std::borrow::Cow::Borrowed("blocked: you are banned"),
        };
        keeper.on_relay_message(&client, &url, &closed).await;

        assert!(
            health.is_backing_off(&url),
            "a CLOSED the SDK removes the subscription for must still arm the keeper"
        );
        assert!(
            !acked(&health, &url),
            "the replacement REQ has yet to be answered, so the old EOSE cannot vouch for it"
        );

        relay.shutdown();
    }

    #[tokio::test]
    async fn frames_for_other_subscriptions_are_ignored() {
        let client = crate::util::mostro_nostr_client_options(None).build();
        let (keeper, health) = keeper();
        let relay = relay_url("ws://relay.example");

        // Mostro's price provider and NIP-33 queries share these relays; their
        // CLOSED frames must not touch the inbox's state.
        let other = RelayMessage::Closed {
            subscription_id: std::borrow::Cow::Owned(SubscriptionId::new("someone-else")),
            message: std::borrow::Cow::Borrowed("error: not yours"),
        };
        keeper.on_relay_message(&client, &relay, &other).await;

        assert!(
            !health.is_backing_off(&relay),
            "a CLOSED for another subscription must not be treated as an inbox failure"
        );
    }

    // ────────────────────────── end-to-end regression ─────────────────────────

    /// Rejects the first REQ it sees and admits every one after it: a relay
    /// having a bad moment, which is exactly the case the daemon used to never
    /// recover from.
    #[derive(Debug, Default)]
    struct RejectFirstQuery {
        seen: std::sync::atomic::AtomicUsize,
    }

    impl nostr_sdk::local_relay::QueryPolicy for RejectFirstQuery {
        fn admit_query<'a>(
            &'a self,
            _query: &'a mut Filter,
            _addr: &'a std::net::SocketAddr,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = nostr_sdk::local_relay::QueryPolicyResult>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                let first = self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                if first {
                    nostr_sdk::local_relay::QueryPolicyResult::reject(
                        MachineReadablePrefix::Error,
                        "subscription refused",
                    )
                } else {
                    nostr_sdk::local_relay::QueryPolicyResult::Accept
                }
            })
        }
    }

    /// A gift wrap addressed to `recipient` — one p-tag, as the transport (and
    /// the relay's own validation) requires.
    fn wrap_for(recipient: PublicKey) -> Event {
        EventBuilder::new(Kind::GiftWrap, "sealed")
            .tag(Tag::public_key(recipient))
            .finalize(&Keys::generate())
            .expect("sign gift wrap")
    }

    #[tokio::test]
    async fn closed_inbox_is_resubscribed_and_hears_again() {
        use futures::StreamExt;
        use nostr_sdk::local_relay::LocalRelay;

        let relay = LocalRelay::builder()
            .query_policy(RejectFirstQuery::default())
            .build();
        relay.run().await.expect("run local relay");
        let url = relay.url().await;

        let mostro = Keys::generate();
        let subscription = InboxSubscription::new(mostro.public_key(), Kind::GiftWrap);

        let client = crate::util::mostro_nostr_client_options(None).build();
        client.add_relay(url.clone()).await.expect("add_relay");
        client.connect().await;

        let mut notifications = client.notifications();
        // The relay CLOSEs this one; without the keeper the ear is gone here.
        subscription.subscribe(&client).await.expect("subscribe");

        let publisher = ClientBuilder::default().build();
        publisher.add_relay(url.clone()).await.expect("add_relay");
        publisher.connect().await;

        let keeper = InboxKeeper::new(subscription.clone());
        let wanted = wrap_for(mostro.public_key());
        let wanted_id = wanted.id;
        let mut published = false;

        let heard = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(notification) = notifications.next().await {
                match notification {
                    ClientNotification::Event { event, .. } => {
                        if event.id == wanted_id {
                            return true;
                        }
                    }
                    ClientNotification::Message { relay_url, message } => {
                        keeper.on_relay_message(&client, &relay_url, &message).await;
                        // Publish only once the inbox is confirmed live again,
                        // so the event cannot be mistaken for stored history.
                        if !published && matches!(&*message, RelayMessage::EndOfStoredEvents(_)) {
                            published = true;
                            publisher.send_event(&wanted).await.expect("publish");
                        }
                    }
                    ClientNotification::Shutdown => return false,
                }
            }
            false
        })
        .await
        .expect("timed out: the inbox never recovered from the relay's CLOSED");

        assert!(
            heard,
            "after a relay CLOSED the inbox, Mostro must re-subscribe and receive again"
        );

        relay.shutdown();
    }

    #[tokio::test]
    async fn a_receiver_created_after_the_req_misses_its_eose() {
        use futures::StreamExt;
        use nostr_sdk::local_relay::LocalRelay;

        // Why the event loop must subscribe *after* taking its notification
        // stream: the SDK delivers nothing that predates the receiver, so a
        // REQ sent earlier loses its EOSE — and any event arriving meanwhile.
        let relay = LocalRelay::builder().build();
        relay.run().await.expect("run local relay");
        let url = relay.url().await;

        let subscription = InboxSubscription::new(pubkey(), Kind::GiftWrap);
        let client = crate::util::mostro_nostr_client_options(None).build();
        client.add_relay(url.clone()).await.expect("add_relay");
        client.connect().await;

        // Subscribe first, listen second — the order this module avoids.
        subscription.subscribe(&client).await.expect("subscribe");
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut late = client.notifications();

        let saw_eose = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(notification) = late.next().await {
                if let ClientNotification::Message { message, .. } = notification {
                    if let RelayMessage::EndOfStoredEvents(id) = &*message {
                        if id.as_ref() == subscription.id() {
                            return true;
                        }
                    }
                }
            }
            false
        })
        .await
        .unwrap_or(false);

        assert!(
            !saw_eose,
            "SDK behaviour changed: a late receiver now sees earlier frames, so the \
             subscribe-after-stream ordering in `app::run` could be relaxed"
        );

        relay.shutdown();
    }

    #[tokio::test]
    async fn without_the_keeper_a_closed_inbox_stays_dead() {
        use nostr_sdk::local_relay::LocalRelay;

        // The defect this module exists for: the SDK drops a CLOSED
        // subscription outright, and nothing re-issues the REQ. Pinned here so
        // that a future SDK bump changing this behaviour is noticed.
        let relay = LocalRelay::builder()
            .query_policy(RejectFirstQuery::default())
            .build();
        relay.run().await.expect("run local relay");
        let url = relay.url().await;

        let subscription = InboxSubscription::new(pubkey(), Kind::GiftWrap);
        let client = crate::util::mostro_nostr_client_options(None).build();
        client.add_relay(url.clone()).await.expect("add_relay");
        client.connect().await;
        subscription.subscribe(&client).await.expect("subscribe");

        // Give the relay time to answer with CLOSED.
        tokio::time::sleep(Duration::from_secs(2)).await;

        assert!(
            !client.subscriptions().await.contains_key(subscription.id()),
            "SDK behaviour changed: a CLOSED subscription is no longer dropped, \
             so the keeper's premise needs revisiting"
        );

        relay.shutdown();
    }

    // ──────────────────────────────── watchdog ────────────────────────────────

    #[tokio::test]
    async fn watchdog_resubscribes_a_relay_that_lost_the_inbox() {
        use nostr_sdk::local_relay::LocalRelay;

        // Accepts every REQ: the point here is the *missing* subscription, not
        // a refusing relay.
        let relay = LocalRelay::builder().build();
        relay.run().await.expect("run local relay");
        let url = relay.url().await;

        let subscription = InboxSubscription::new(pubkey(), Kind::GiftWrap);
        let client = crate::util::mostro_nostr_client_options(None).build();
        client.add_relay(url.clone()).await.expect("add_relay");
        client.connect().await;
        subscription.subscribe(&client).await.expect("subscribe");
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Simulate the ear vanishing without a CLOSED the loop could see —
        // a frame dropped by a lagging notification channel looks like this.
        client
            .unsubscribe(subscription.id())
            .await
            .expect("drop the subscription");
        assert!(!client.subscriptions().await.contains_key(subscription.id()));

        let health = Arc::new(InboxHealth::at(T0));

        // The audit re-subscribes, but does not yet claim to be listening: a
        // REQ that just went out proves nothing about whether the relay will
        // honour it.
        assert_eq!(
            check_inbox_health_with(&client, &subscription, Some(health.clone())).await,
            InboxStatus::Blind,
            "a relay re-subscribed during this audit must not count as listening yet"
        );
        assert!(
            client.subscriptions().await.contains_key(subscription.id()),
            "the inbox subscription must be back after the audit"
        );

        // The relay answers the new REQ; in the daemon this is the event loop
        // seeing the EOSE and recording it.
        health.note_relay_acknowledged(&url);

        assert_eq!(
            check_inbox_health_with(&client, &subscription, Some(health)).await,
            InboxStatus::Listening,
            "a relay that answered the REQ is serving the inbox"
        );

        relay.shutdown();
    }

    #[tokio::test]
    async fn a_closed_frame_invalidates_the_relays_earlier_acknowledgement() {
        use nostr_sdk::local_relay::LocalRelay;

        // A relay can answer the REQ, later close the subscription, and then
        // ignore the replacement. Its old EOSE must not carry over: the
        // watchdog would see the re-registered subscription plus stale credit
        // and resume order timeouts while the node is deaf.
        let relay = LocalRelay::builder().build();
        relay.run().await.expect("run local relay");
        let url = relay.url().await;

        let client = crate::util::mostro_nostr_client_options(None).build();
        client.add_relay(url.clone()).await.expect("add_relay");
        client.connect().await;

        let health = Arc::new(InboxHealth::at(T0));
        let subscription = InboxSubscription::new(pubkey(), Kind::GiftWrap);
        let keeper = InboxKeeper::with_health(subscription.clone(), Some(health.clone()));

        // The relay answered an earlier REQ.
        health.note_relay_acknowledged(&url);
        assert!(acked(&health, &url));

        // Now it closes the subscription; the keeper re-sends the REQ.
        let closed = RelayMessage::Closed {
            subscription_id: std::borrow::Cow::Owned(subscription.id().clone()),
            message: std::borrow::Cow::Borrowed("error: go away"),
        };
        keeper.on_relay_message(&client, &url, &closed).await;

        assert!(
            !acked(&health, &url),
            "an EOSE from before the CLOSED must not vouch for the replacement REQ"
        );

        relay.shutdown();
    }

    #[tokio::test]
    async fn watchdog_does_not_trust_an_acknowledgement_from_a_previous_connection() {
        use nostr_sdk::local_relay::LocalRelay;

        // The audit-level counterpart: the subscription is registered and the
        // relay has an acknowledgement on file, but it predates the current
        // websocket session, which is exactly what a reconnect leaves behind.
        let relay = LocalRelay::builder().build();
        relay.run().await.expect("run local relay");
        let url = relay.url().await;

        let subscription = InboxSubscription::new(pubkey(), Kind::GiftWrap);
        let client = crate::util::mostro_nostr_client_options(None).build();
        client.add_relay(url.clone()).await.expect("add_relay");
        client.connect().await;
        subscription.subscribe(&client).await.expect("subscribe");
        tokio::time::sleep(Duration::from_millis(500)).await;

        let health = Arc::new(InboxHealth::at(T0));
        health.note_relay_acknowledged_at(&url, T0);

        assert!(
            client.subscriptions().await.contains_key(subscription.id()),
            "precondition: the SDK has the subscription registered"
        );
        assert_eq!(
            check_inbox_health_with(&client, &subscription, Some(health.clone())).await,
            InboxStatus::Blind,
            "a stale acknowledgement plus a live registration must not read as listening"
        );

        // An EOSE on the current connection is what settles it.
        health.note_relay_acknowledged(&url);
        assert_eq!(
            check_inbox_health_with(&client, &subscription, Some(health)).await,
            InboxStatus::Listening
        );

        relay.shutdown();
    }

    #[tokio::test]
    async fn watchdog_does_not_trust_a_relay_that_never_answered() {
        use nostr_sdk::local_relay::LocalRelay;

        // A relay can hold the connection open and quietly drop the REQ. The
        // SDK still lists the subscription, because that map records what
        // Mostro sent, not what the relay agreed to serve. Treating it as
        // proof would resume order timeouts against a deaf node.
        let relay = LocalRelay::builder().build();
        relay.run().await.expect("run local relay");
        let url = relay.url().await;

        let subscription = InboxSubscription::new(pubkey(), Kind::GiftWrap);
        let client = crate::util::mostro_nostr_client_options(None).build();
        client.add_relay(url.clone()).await.expect("add_relay");
        client.connect().await;
        subscription.subscribe(&client).await.expect("subscribe");
        tokio::time::sleep(Duration::from_millis(500)).await;

        let health = Arc::new(InboxHealth::at(T0));

        assert!(
            client.subscriptions().await.contains_key(subscription.id()),
            "precondition: the SDK has the subscription registered"
        );
        assert_eq!(
            check_inbox_health_with(&client, &subscription, Some(health.clone())).await,
            InboxStatus::Blind,
            "local registration alone must not count as the relay serving the inbox"
        );

        // Once it does answer, the same registration is finally evidence.
        health.note_relay_acknowledged(&url);
        assert_eq!(
            check_inbox_health_with(&client, &subscription, Some(health)).await,
            InboxStatus::Listening
        );

        relay.shutdown();
    }

    #[tokio::test]
    async fn keeper_records_the_acknowledgement_the_watchdog_reads() {
        // The EOSE is only observable from the event loop, while the watchdog
        // is what acts on it — this is the handoff between the two.
        let client = crate::util::mostro_nostr_client_options(None).build();
        let health = Arc::new(InboxHealth::at(T0));
        let subscription = InboxSubscription::new(pubkey(), Kind::GiftWrap);
        let keeper = InboxKeeper::with_health(subscription.clone(), Some(health.clone()));
        let url = relay_url("ws://relay.example");

        assert!(!acked(&health, &url));

        let eose =
            RelayMessage::EndOfStoredEvents(std::borrow::Cow::Owned(subscription.id().clone()));
        keeper.on_relay_message(&client, &url, &eose).await;

        assert!(
            acked(&health, &url),
            "an EOSE for the inbox must be recorded as the relay serving it"
        );

        // A frame for someone else's subscription proves nothing about ours.
        let other_url = relay_url("ws://other.example");
        let other = RelayMessage::EndOfStoredEvents(std::borrow::Cow::Owned(SubscriptionId::new(
            "not-the-inbox",
        )));
        keeper.on_relay_message(&client, &other_url, &other).await;
        assert!(!acked(&health, &other_url));
    }

    #[tokio::test]
    async fn watchdog_stays_blind_against_a_relay_that_keeps_closing() {
        use nostr_sdk::local_relay::LocalRelay;

        /// Refuses every REQ, always.
        #[derive(Debug)]
        struct RejectAllQueries;

        impl nostr_sdk::local_relay::QueryPolicy for RejectAllQueries {
            fn admit_query<'a>(
                &'a self,
                _query: &'a mut Filter,
                _addr: &'a std::net::SocketAddr,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = nostr_sdk::local_relay::QueryPolicyResult>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(async {
                    nostr_sdk::local_relay::QueryPolicyResult::reject(
                        MachineReadablePrefix::Blocked,
                        "no subscriptions here",
                    )
                })
            }
        }

        let relay = LocalRelay::builder().query_policy(RejectAllQueries).build();
        relay.run().await.expect("run local relay");
        let url = relay.url().await;

        let subscription = InboxSubscription::new(pubkey(), Kind::GiftWrap);
        let client = crate::util::mostro_nostr_client_options(None).build();
        client.add_relay(url.clone()).await.expect("add_relay");
        client.connect().await;
        subscription.subscribe(&client).await.expect("subscribe");

        let health = Arc::new(InboxHealth::at(T0));

        // However many rounds it runs, a relay that keeps closing the inbox
        // never makes the node look healthy — this is what keeps the timeout
        // machinery paused while trade messages are being lost.
        for round in 0..3 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert_eq!(
                check_inbox_health_with(&client, &subscription, Some(health.clone())).await,
                InboxStatus::Blind,
                "round {round}: a relay that refuses every REQ must never read as listening"
            );
        }

        // The audit draws on the same budget the event loop does, so a relay
        // that refuses on principle is paced towards `RESUBSCRIBE_MAX_BACKOFF`
        // instead of being handed a REQ on every pass, forever. Skipping the
        // retry must not soften the verdict: the node is still deaf here.
        assert!(
            health.is_backing_off(&url),
            "repeated refusals must accumulate on the shared re-subscribe budget"
        );
        assert!(!health.allow_resubscribe(&url));
        assert_eq!(
            check_inbox_health_with(&client, &subscription, Some(health)).await,
            InboxStatus::Blind,
            "a pass that backs off instead of re-sending must still report the inbox deaf"
        );

        relay.shutdown();
    }

    #[tokio::test]
    async fn watchdog_reports_blind_when_no_relay_serves_the_inbox() {
        // A client with no relays at all is the limit case of every relay
        // being down: nothing can deliver a trade message.
        let client = crate::util::mostro_nostr_client_options(None).build();
        let subscription = InboxSubscription::new(pubkey(), Kind::GiftWrap);

        assert_eq!(
            check_inbox_health(&client, &subscription).await,
            InboxStatus::Blind
        );
    }

    #[tokio::test]
    async fn watchdog_ignores_a_disconnected_relay() {
        use nostr_sdk::local_relay::LocalRelay;

        let live = LocalRelay::builder().build();
        live.run().await.expect("run local relay");
        let live_url = live.url().await;

        let client = crate::util::mostro_nostr_client_options(None).build();
        client.add_relay(live_url.clone()).await.expect("add_relay");
        // Never connects: a relay that is down must not be re-subscribed on
        // every tick, nor drag the verdict to blind while another one serves.
        client
            .add_relay("ws://127.0.0.1:1")
            .await
            .expect("add_relay");
        client.connect().await;

        let subscription = InboxSubscription::new(pubkey(), Kind::GiftWrap);
        subscription.subscribe(&client).await.expect("subscribe");
        tokio::time::sleep(Duration::from_millis(500)).await;

        assert_eq!(
            check_inbox_health(&client, &subscription).await,
            InboxStatus::Listening,
            "one healthy relay is enough to keep hearing"
        );

        live.shutdown();
    }
}
