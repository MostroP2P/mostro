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
//! that are rejected on arrival.

use nostr_sdk::prelude::*;
use tracing::{error, info, warn};

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

#[cfg(test)]
mod tests {
    use super::*;

    fn pubkey() -> PublicKey {
        Keys::generate().public_key()
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
}
