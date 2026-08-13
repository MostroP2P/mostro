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

use std::collections::HashMap;
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

        // A `CLOSED` does not always remove the subscription: rate-limited and
        // auth-required closures only *mark* it, and a marked subscription is
        // re-REQ'd no earlier than the next reconnect — which may never come on
        // a healthy connection. Dropping the registration first makes the REQ
        // below unconditional, instead of being refused as a duplicate id.
        let _ = relay.unsubscribe(self.subscription.id()).await;

        match relay
            .subscribe(self.subscription.filter().clone())
            .with_id(self.subscription.id().clone())
            .await
        {
            Ok(_) => info!("Re-sent the inbox subscription to relay {relay_url}"),
            Err(e) => warn!("Failed to re-subscribe the inbox on relay {relay_url}: {e}"),
        }
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
        let client = crate::util::mostro_nostr_client_options().build();
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
        let client = crate::util::mostro_nostr_client_options().build();
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

        let client = crate::util::mostro_nostr_client_options().build();
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
        let client = crate::util::mostro_nostr_client_options().build();
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
}
