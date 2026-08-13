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
//! Not every way of losing the ear announces itself with a frame, though: the
//! notification channel silently drops messages when the consumer falls
//! behind, a REQ can fail to go out, a relay can be added after startup.
//! [`check_inbox_health`] is the backstop — it asks each connected relay
//! whether it is still serving the subscription, re-subscribes the ones that
//! are not, and records the verdict in [`InboxHealth`] so the rest of the
//! daemon can tell whether Mostro is currently able to hear anything at all.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use nostr_sdk::prelude::*;
use tracing::{debug, error, info, warn};

/// Subscription id used for the daemon inbox.
///
/// Fixed rather than the SDK's per-call random id, so a `CLOSED` frame can be
/// attributed to the inbox and the REQ re-issued under the same name. It is
/// visible to every relay, which costs nothing in privacy: the filter's `#p`
/// tag already names this node.
const INBOX_SUBSCRIPTION_ID: &str = "mostro-inbox";

/// Delay before a *second* consecutive re-subscribe to the same relay.
///
/// The first `CLOSED` is answered immediately — the common case is a transient
/// refusal, and every second of delay is a second of deaf node. Backoff only
/// starts mattering when a relay keeps closing the inbox.
const RESUBSCRIBE_INITIAL_BACKOFF: Duration = Duration::from_secs(2);

/// Ceiling for the per-relay re-subscribe delay.
///
/// A relay that has refused the inbox for five minutes straight is not having
/// a hiccup — it is configured to refuse us (NIP-42, a pubkey allowlist, a ban)
/// and the operator has to intervene. Retrying every five minutes keeps the
/// door open for a config change on their side without generating traffic that
/// looks like an attack.
const RESUBSCRIBE_MAX_BACKOFF: Duration = Duration::from_secs(300);

/// Per-relay re-subscribe pacing.
#[derive(Debug)]
struct RelayBackoff {
    /// Earliest instant at which another REQ may go out to this relay.
    next_attempt_at: Instant,
    /// Delay applied after the next attempt; doubles up to the ceiling.
    delay: Duration,
}

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
/// stream — hence the plain `&mut self` state rather than a lock.
pub struct InboxKeeper {
    subscription: InboxSubscription,
    /// Only holds relays that are currently failing; a relay that accepts the
    /// REQ is dropped from the map, so the steady state is empty.
    backoff: HashMap<RelayUrl, RelayBackoff>,
}

impl InboxKeeper {
    pub fn new(subscription: InboxSubscription) -> Self {
        Self {
            subscription,
            backoff: HashMap::new(),
        }
    }

    /// React to one control-plane frame from `relay_url`.
    ///
    /// Two frames matter for the inbox: `CLOSED`, which means the ear on that
    /// relay is gone and has to be re-opened, and `EOSE`, which is a relay
    /// confirming it accepted the REQ and is the signal used to clear the
    /// backoff. Everything else (`OK`, `NOTICE`, other subscriptions' frames)
    /// is not this module's business.
    pub async fn on_relay_message(
        &mut self,
        client: &Client,
        relay_url: &RelayUrl,
        message: &RelayMessage<'_>,
    ) {
        match message {
            RelayMessage::Closed {
                subscription_id,
                message,
            } if subscription_id.as_ref() == self.subscription.id() => {
                warn!("Relay {relay_url} closed the Mostro inbox subscription: \"{message}\"");
                self.resubscribe(client, relay_url).await;
            }
            RelayMessage::EndOfStoredEvents(subscription_id)
                if subscription_id.as_ref() == self.subscription.id() =>
            {
                // The relay answered the REQ, so whatever made it fail before
                // is over and the next failure deserves a prompt retry again.
                if self.backoff.remove(relay_url).is_some() {
                    info!("Inbox subscription re-established on relay {relay_url}");
                }
            }
            _ => {}
        }
    }

    /// Re-issue the inbox REQ to a single relay, subject to backoff.
    async fn resubscribe(&mut self, client: &Client, relay_url: &RelayUrl) {
        if !self.allow_attempt(relay_url, Instant::now()) {
            debug!("Skipping inbox re-subscribe on relay {relay_url}: backing off");
            return;
        }

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

        resubscribe_relay(&relay, &self.subscription).await;
    }

    /// Whether a re-subscribe to `relay` may go out at `now`, arming the next
    /// delay when it may. The first failure for a relay always passes.
    fn allow_attempt(&mut self, relay: &RelayUrl, now: Instant) -> bool {
        match self.backoff.get_mut(relay) {
            None => {
                self.backoff.insert(
                    relay.clone(),
                    RelayBackoff {
                        next_attempt_at: now + RESUBSCRIBE_INITIAL_BACKOFF,
                        delay: RESUBSCRIBE_INITIAL_BACKOFF,
                    },
                );
                true
            }
            Some(state) => {
                if now < state.next_attempt_at {
                    return false;
                }
                state.delay = (state.delay * 2).min(RESUBSCRIBE_MAX_BACKOFF);
                state.next_attempt_at = now + state.delay;
                true
            }
        }
    }
}

/// Re-send the inbox REQ to one relay.
///
/// Shared by the event-loop keeper (reacting to a `CLOSED`) and the watchdog
/// (finding an ear that went missing without one), so both recover a relay the
/// same way.
async fn resubscribe_relay(relay: &Relay, subscription: &InboxSubscription) {
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
}

/// Whether the daemon can currently hear anything at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxStatus {
    /// At least one connected relay is serving the inbox subscription.
    Listening,
    /// No connected relay is serving it: every message sent to Mostro right
    /// now is being lost.
    Blind,
}

/// Process-wide inbox health. `None` until [`InboxHealth::install_global`]
/// runs at startup; consumers treat an absent health record as "listening", so
/// unit tests that never install it behave as before.
static INBOX_HEALTH: OnceLock<InboxHealth> = OnceLock::new();

/// Why [`InboxHealth::install_global`] refused. Mirrors `spam_gate::InstallError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallError {
    /// A health record is already installed.
    AlreadyInstalled,
}

#[derive(Debug, Default)]
struct HealthState {
    /// When the current outage began, if the inbox is deaf right now.
    blind_since: Option<Instant>,
    /// Debt owed by finished outages, exact as of `owed_as_of`. It wears off
    /// as time passes with the inbox listening, and is frozen while it is
    /// blind — repaying it requires time the user could actually talk in.
    owed: Duration,
    /// When `owed` was last exact. `None` before the first outage.
    owed_as_of: Option<Instant>,
}

/// Tracks whether the daemon's ear is open, and for how long it was not.
///
/// The scheduler's timeout machinery reads this: an order is only "late" if
/// Mostro was in a position to hear from the user, and the ten-second replay
/// window means a message sent into a dead inbox is gone rather than delayed
/// (see the module docs). Punishing a user for a silence the node itself
/// caused would be unfair, so the timeout clock stops while the inbox is down.
#[derive(Debug, Default)]
pub struct InboxHealth {
    state: Mutex<HealthState>,
}

impl InboxHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install as the process-wide health record.
    pub fn install_global(self) -> Result<(), InstallError> {
        INBOX_HEALTH
            .set(self)
            .map_err(|_| InstallError::AlreadyInstalled)
    }

    /// The process-wide health record, if one was installed.
    pub fn global() -> Option<&'static InboxHealth> {
        INBOX_HEALTH.get()
    }

    /// Record the current observation, returning the resulting status.
    fn observe(&self, status: InboxStatus, now: Instant) -> InboxStatus {
        let mut state = self.state.lock().expect("inbox health mutex poisoned");
        match (status, state.blind_since) {
            (InboxStatus::Blind, None) => {
                // Freeze whatever an earlier outage still owes: from here on,
                // no time passes that a user could have used to answer, so
                // none of that debt gets repaid.
                state.owed = settled_debt(&state, now);
                state.owed_as_of = Some(now);
                state.blind_since = Some(now);
            }
            (InboxStatus::Listening, Some(since)) => {
                // The frozen debt plus this outage; two outages in quick
                // succession add up rather than cancelling each other out.
                state.owed += now.saturating_duration_since(since);
                state.owed_as_of = Some(now);
                state.blind_since = None;
            }
            _ => {}
        }
        status
    }

    /// Whether the inbox is deaf right now.
    pub fn is_blind(&self) -> bool {
        self.state
            .lock()
            .expect("inbox health mutex poisoned")
            .blind_since
            .is_some()
    }

    /// How much time the order-timeout clock currently owes users.
    ///
    /// Deadlines are measured against wall time, but a user cannot answer a
    /// node that cannot hear — and because a message sent into a dead inbox is
    /// lost rather than queued (the ten-second replay window, see the module
    /// docs), they have to send it again once the ear is back. So the time the
    /// inbox spent down is given back: the debt equals the outage when it ends
    /// and decays to zero over an equal span, which is the same as saying the
    /// timeout clock stood still while Mostro was deaf.
    ///
    /// Compensation is granted to every waiting order rather than only to
    /// those in flight during the outage — an order taken *during* the blind
    /// window gets slightly more grace than it strictly lost. That error is
    /// deliberate: it delays a cancellation, where the opposite would slash an
    /// honest user's bond for the node's own failure.
    pub fn timeout_debt(&self) -> Duration {
        self.timeout_debt_at(Instant::now())
    }

    fn timeout_debt_at(&self, now: Instant) -> Duration {
        let state = self.state.lock().expect("inbox health mutex poisoned");
        match state.blind_since {
            // Still deaf: the debt frozen at the start of this outage, plus
            // every second it has run for.
            Some(since) => state.owed + now.saturating_duration_since(since),
            None => settled_debt(&state, now),
        }
    }
}

/// The debt still outstanding at `now` while the inbox is listening: it starts
/// at the recorded amount and wears off second for second.
fn settled_debt(state: &HealthState, now: Instant) -> Duration {
    match state.owed_as_of {
        Some(as_of) => state
            .owed
            .saturating_sub(now.saturating_duration_since(as_of)),
        None => Duration::ZERO,
    }
}

/// Check every read relay, re-subscribing any that lost the inbox, and record
/// the verdict in the process-wide [`InboxHealth`].
///
/// The health question is asked of the *subscription*, not of traffic: a node
/// with no trades in flight is legitimately silent, so treating quiet as
/// failure would raise false alarms on an idle instance and, worse, would stop
/// the timeout machinery for no reason.
///
/// A relay re-subscribed during this very audit does **not** count as
/// listening. Sending a REQ is not the same as having it honoured — a relay
/// that closes the inbox on principle would accept the REQ and close it again
/// moments later, and counting the attempt would report a healthy inbox
/// forever while nothing was ever delivered. Only a subscription that was
/// already in place when the audit ran proves the relay kept it. Recovery is
/// therefore confirmed on the following round, which costs one extra interval
/// before the inbox is declared healthy again and keeps the error on the safe
/// side: the timeout clock stays frozen a little longer than strictly needed,
/// rather than resuming while the node is still deaf.
pub async fn check_inbox_health(client: &Client, subscription: &InboxSubscription) -> InboxStatus {
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
        if relay.subscription(subscription.id()).await.is_some() {
            listening += 1;
        } else {
            // Connected but not subscribed: a CLOSED the event loop never saw
            // (the notification channel drops frames when it lags), a REQ that
            // failed to go out, or a relay re-added after startup.
            warn!("Relay {url} is connected but not serving the Mostro inbox; re-subscribing");
            resubscribe_relay(relay, subscription).await;
            retried += 1;
        }
    }

    let status = if listening > 0 {
        InboxStatus::Listening
    } else {
        InboxStatus::Blind
    };

    if let Some(health) = InboxHealth::global() {
        let was_blind = health.is_blind();
        health.observe(status, Instant::now());

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
    use super::*;

    fn pubkey() -> PublicKey {
        Keys::generate().public_key()
    }

    fn keeper() -> InboxKeeper {
        InboxKeeper::new(InboxSubscription::new(pubkey(), Kind::GiftWrap))
    }

    fn relay_url(url: &str) -> RelayUrl {
        RelayUrl::parse(url).expect("valid relay url")
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

    // ───────────────────────────── backoff pacing ─────────────────────────────

    #[test]
    fn first_closure_from_a_relay_retries_immediately() {
        let mut keeper = keeper();
        let relay = relay_url("ws://relay.example");

        assert!(
            keeper.allow_attempt(&relay, Instant::now()),
            "a first CLOSED must be answered at once: every delay is deaf time"
        );
    }

    #[test]
    fn repeat_closures_are_paced_and_back_off() {
        let mut keeper = keeper();
        let relay = relay_url("ws://relay.example");
        let start = Instant::now();

        assert!(keeper.allow_attempt(&relay, start));
        // A relay that closes again right away must not pull a second REQ.
        assert!(!keeper.allow_attempt(&relay, start));
        assert!(!keeper.allow_attempt(&relay, start + Duration::from_secs(1)));

        // Past the first delay it retries, and the next wait is longer.
        assert!(keeper.allow_attempt(&relay, start + RESUBSCRIBE_INITIAL_BACKOFF));
        assert!(!keeper.allow_attempt(&relay, start + RESUBSCRIBE_INITIAL_BACKOFF * 2));
        assert!(keeper.allow_attempt(&relay, start + RESUBSCRIBE_INITIAL_BACKOFF * 3));
    }

    #[test]
    fn backoff_is_capped() {
        let mut keeper = keeper();
        let relay = relay_url("ws://relay.example");
        let mut now = Instant::now();

        // Drive it well past the ceiling.
        for _ in 0..20 {
            assert!(keeper.allow_attempt(&relay, now));
            now += RESUBSCRIBE_MAX_BACKOFF * 2;
        }

        assert_eq!(
            keeper.backoff.get(&relay).expect("state kept").delay,
            RESUBSCRIBE_MAX_BACKOFF,
            "a hostile relay must still be retried every {RESUBSCRIBE_MAX_BACKOFF:?}"
        );
    }

    #[test]
    fn backoff_is_per_relay() {
        let mut keeper = keeper();
        let hostile = relay_url("ws://hostile.example");
        let healthy = relay_url("ws://healthy.example");
        let now = Instant::now();

        assert!(keeper.allow_attempt(&hostile, now));
        assert!(!keeper.allow_attempt(&hostile, now));
        // One misbehaving relay must not delay recovery on another.
        assert!(keeper.allow_attempt(&healthy, now));
    }

    // ───────────────────────── control-plane handling ─────────────────────────

    #[tokio::test]
    async fn eose_for_the_inbox_clears_the_backoff() {
        let client = crate::util::mostro_nostr_client_options(None).build();
        let mut keeper = keeper();
        let relay = relay_url("ws://relay.example");
        let now = Instant::now();

        assert!(keeper.allow_attempt(&relay, now));
        assert!(keeper.backoff.contains_key(&relay));

        let eose = RelayMessage::EndOfStoredEvents(std::borrow::Cow::Owned(
            keeper.subscription.id().clone(),
        ));
        keeper.on_relay_message(&client, &relay, &eose).await;

        assert!(
            !keeper.backoff.contains_key(&relay),
            "an accepted REQ must reset the pacing for the next failure"
        );
    }

    #[tokio::test]
    async fn frames_for_other_subscriptions_are_ignored() {
        let client = crate::util::mostro_nostr_client_options(None).build();
        let mut keeper = keeper();
        let relay = relay_url("ws://relay.example");

        // Mostro's price provider and NIP-33 queries share these relays; their
        // CLOSED frames must not touch the inbox's state.
        let other = RelayMessage::Closed {
            subscription_id: std::borrow::Cow::Owned(SubscriptionId::new("someone-else")),
            message: std::borrow::Cow::Borrowed("error: not yours"),
        };
        keeper.on_relay_message(&client, &relay, &other).await;

        assert!(
            keeper.backoff.is_empty(),
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

        let mut keeper = InboxKeeper::new(subscription.clone());
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

    #[test]
    fn health_records_an_outage_from_first_blindness_to_recovery() {
        let health = InboxHealth::new();
        let start = Instant::now();

        assert!(!health.is_blind(), "a fresh record starts out listening");

        health.observe(InboxStatus::Blind, start);
        assert!(health.is_blind());

        // Staying blind must not restart the clock — the outage began at the
        // first observation, and that is what the timeout discount is owed on.
        health.observe(InboxStatus::Blind, start + Duration::from_secs(30));
        assert!(health.is_blind());

        health.observe(InboxStatus::Listening, start + Duration::from_secs(90));
        assert!(!health.is_blind());

        assert_eq!(
            health.timeout_debt_at(start + Duration::from_secs(90)),
            Duration::from_secs(90),
            "the recorded outage must span the whole blind window"
        );
    }

    #[test]
    fn a_node_that_was_never_blind_owes_nothing() {
        let health = InboxHealth::new();
        assert_eq!(health.timeout_debt_at(Instant::now()), Duration::ZERO);
    }

    #[test]
    fn debt_grows_while_blind_and_wears_off_after_recovery() {
        let health = InboxHealth::new();
        let start = Instant::now();

        health.observe(InboxStatus::Blind, start);

        // While deaf, the clock is stopped: the debt is the whole outage so far.
        assert_eq!(
            health.timeout_debt_at(start + Duration::from_secs(120)),
            Duration::from_secs(120)
        );

        health.observe(InboxStatus::Listening, start + Duration::from_secs(300));

        // On recovery, users are owed the full outage...
        assert_eq!(
            health.timeout_debt_at(start + Duration::from_secs(300)),
            Duration::from_secs(300)
        );
        // ...which then decays second for second, so a five-minute outage
        // gives back five minutes and no more.
        assert_eq!(
            health.timeout_debt_at(start + Duration::from_secs(400)),
            Duration::from_secs(200)
        );
        assert_eq!(
            health.timeout_debt_at(start + Duration::from_secs(600)),
            Duration::ZERO
        );
        assert_eq!(
            health.timeout_debt_at(start + Duration::from_secs(10_000)),
            Duration::ZERO,
            "the debt must not linger once repaid"
        );
    }

    #[test]
    fn consecutive_outages_accumulate_their_debt() {
        let health = InboxHealth::new();
        let start = Instant::now();

        // A 100s outage, recovered at t=100 — debt 100s.
        health.observe(InboxStatus::Blind, start);
        health.observe(InboxStatus::Listening, start + Duration::from_secs(100));

        // A second outage begins at t=140, when 60s of the first is still owed,
        // and lasts 50s.
        health.observe(InboxStatus::Blind, start + Duration::from_secs(140));
        health.observe(InboxStatus::Listening, start + Duration::from_secs(190));

        assert_eq!(
            health.timeout_debt_at(start + Duration::from_secs(190)),
            Duration::from_secs(110),
            "the second outage must not wipe out what the first still owed"
        );
    }

    #[test]
    fn health_ignores_repeated_healthy_observations() {
        let health = InboxHealth::new();
        let now = Instant::now();

        health.observe(InboxStatus::Listening, now);
        health.observe(InboxStatus::Listening, now + Duration::from_secs(30));

        assert!(!health.is_blind());
        assert_eq!(
            health.timeout_debt_at(now + Duration::from_secs(30)),
            Duration::ZERO,
            "a node that was never blind has no outage to compensate for"
        );
    }

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

        // The audit re-subscribes, but does not yet claim to be listening: a
        // REQ that just went out proves nothing about whether the relay will
        // honour it.
        assert_eq!(
            check_inbox_health(&client, &subscription).await,
            InboxStatus::Blind,
            "a relay re-subscribed during this audit must not count as listening yet"
        );
        assert!(
            client.subscriptions().await.contains_key(subscription.id()),
            "the inbox subscription must be back after the audit"
        );

        // The relay kept it, so the next round confirms the recovery.
        assert_eq!(
            check_inbox_health(&client, &subscription).await,
            InboxStatus::Listening,
            "a subscription that survived to the next audit means the ear is open"
        );

        relay.shutdown();
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

        // However many rounds it runs, a relay that keeps closing the inbox
        // never makes the node look healthy — this is what keeps the timeout
        // machinery paused while trade messages are being lost.
        for round in 0..3 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert_eq!(
                check_inbox_health(&client, &subscription).await,
                InboxStatus::Blind,
                "round {round}: a relay that refuses every REQ must never read as listening"
            );
        }

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
