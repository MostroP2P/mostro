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

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
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
///
/// This is a real ceiling because [`check_inbox_health`] draws on the same
/// per-relay budget rather than re-sending on every pass: an audit every
/// `INBOX_WATCHDOG_INTERVAL` would otherwise put a hard floor of thirty
/// seconds under it. The doublings still start well below that interval, so a
/// relay that merely lost the inbox is re-subscribed on the next pass and only
/// a persistently refusing one reaches this figure.
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
                    // The REQ is not the keeper's to re-send: the SDK only
                    // *marks* these two prefixes and re-sends it itself —
                    // after the NIP-42 round-trip for `auth-required`, on the
                    // next reconnect for `rate-limited`. Re-issuing it here
                    // would drop the entry the SDK is about to re-send, race
                    // its AUTH, and arm a backoff against a relay that is
                    // behaving exactly as the protocol says it should.
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
///   claims to be [`RESUBSCRIBE_MAX_BACKOFF`]. Sharing it keeps a transient
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
static INBOX_HEALTH: OnceLock<Arc<InboxHealth>> = OnceLock::new();

/// Why [`InboxHealth::install_global`] refused. Mirrors `spam_gate::InstallError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallError {
    /// A health record is already installed.
    AlreadyInstalled,
}

/// One stretch during which the daemon could not hear.
///
/// Timestamps are wall-clock seconds, the same base as an order's `taken_at`,
/// because that is what these windows are ultimately intersected against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlindWindow {
    start: i64,
    /// `None` while the outage is still running.
    end: Option<i64>,
}

impl BlindWindow {
    /// Seconds of this window that fall inside `[from, to]`.
    fn overlap(&self, from: i64, to: i64, now: i64) -> i64 {
        let end = self.end.unwrap_or(now);
        (end.min(to) - self.start.max(from)).max(0)
    }
}

/// Windows that ended longer ago than this are dropped: no order the timeout
/// job can still be looking at was waiting back then, so they can no longer
/// change any verdict. Generous next to `expiration_seconds` (900s by default)
/// so the bound is never the reason a user loses compensation.
const BLIND_WINDOW_RETENTION_SECS: i64 = 7 * 24 * 3600;

/// Hard cap on retained windows, so a relay flapping in a tight loop cannot
/// grow this unboundedly between prunes.
const MAX_BLIND_WINDOWS: usize = 512;

#[derive(Debug)]
struct HealthState {
    /// The last verdict an audit reached. `None` until the first one runs:
    /// startup is not evidence that the inbox works, and must not be read as
    /// such (see [`InboxHealth::is_confirmed_listening`]).
    verdict: Option<InboxStatus>,
    /// When the record was installed, which is the earliest moment an outage
    /// discovered by the first audit could have begun.
    installed_at: i64,
    /// Every outage this process has seen, oldest first, pruned by age.
    windows: Vec<BlindWindow>,
    /// Relay to the wall-clock second its last `EOSE` for the inbox arrived.
    ///
    /// A relay is only credited with serving the inbox once it says so. The
    /// SDK's own subscription map cannot stand in for this: it records what
    /// *we* sent, so a relay that holds the connection open and quietly
    /// ignores the REQ still looks subscribed there.
    ///
    /// The timestamp is what binds the credit to a single websocket session.
    /// On reconnect the SDK re-sends the REQ by itself, with no frame this
    /// module can observe, so an acknowledgement earned on the previous
    /// connection says nothing about the current one — see
    /// [`InboxHealth::has_acknowledged_since`].
    acknowledged: HashMap<RelayUrl, i64>,
    /// Re-subscribe pacing, drawn on by the event loop and the watchdog alike.
    ///
    /// Only holds relays that are currently failing; a relay that answers the
    /// REQ is dropped from the map, so the steady state is empty.
    backoff: HashMap<RelayUrl, RelayBackoff>,
}

impl HealthState {
    fn blind_now(&self) -> Option<&BlindWindow> {
        self.windows.last().filter(|w| w.end.is_none())
    }
}

/// Tracks whether the daemon's ear is open, and when it was not.
///
/// The scheduler's timeout machinery reads this: an order is only "late" if
/// Mostro was in a position to hear from the user, and the ten-second replay
/// window means a message sent into a dead inbox is gone rather than delayed
/// (see the module docs). Punishing a user for a silence the node itself
/// caused would be unfair, so the timeout clock stops while the inbox is down.
#[derive(Debug)]
pub struct InboxHealth {
    state: Mutex<HealthState>,
}

impl Default for InboxHealth {
    fn default() -> Self {
        Self::new()
    }
}

impl InboxHealth {
    pub fn new() -> Self {
        Self::at(now_secs())
    }

    fn at(installed_at: i64) -> Self {
        Self {
            state: Mutex::new(HealthState {
                verdict: None,
                installed_at,
                windows: Vec::new(),
                acknowledged: HashMap::new(),
                backoff: HashMap::new(),
            }),
        }
    }

    /// Install as the process-wide health record.
    pub fn install_global(self) -> Result<(), InstallError> {
        INBOX_HEALTH
            .set(Arc::new(self))
            .map_err(|_| InstallError::AlreadyInstalled)
    }

    /// The process-wide health record, if one was installed.
    pub fn global() -> Option<Arc<InboxHealth>> {
        INBOX_HEALTH.get().cloned()
    }

    /// Record the current observation, returning the resulting status.
    fn observe(&self, status: InboxStatus, now: i64) -> InboxStatus {
        let mut state = self.state.lock().expect("inbox health mutex poisoned");
        let first_verdict = state.verdict.is_none();
        state.verdict = Some(status);

        match (status, state.blind_now().is_some()) {
            (InboxStatus::Blind, false) => {
                // A first audit that finds the inbox deaf has found an outage
                // that was already running: the node has not heard anything
                // since it came up, so that is when the outage began.
                let start = if first_verdict {
                    state.installed_at
                } else {
                    now
                };
                state.windows.push(BlindWindow { start, end: None });
            }
            (InboxStatus::Listening, true) => {
                if let Some(open) = state.windows.last_mut() {
                    open.end = Some(now);
                }
            }
            _ => {}
        }

        state.windows.retain(|w| match w.end {
            Some(end) => end > now - BLIND_WINDOW_RETENTION_SECS,
            None => true,
        });
        if state.windows.len() > MAX_BLIND_WINDOWS {
            let excess = state.windows.len() - MAX_BLIND_WINDOWS;
            state.windows.drain(..excess);
        }

        status
    }

    /// Whether the inbox is deaf right now.
    ///
    /// A record that has never been audited is not blind — but neither is it
    /// known to be listening, which is the question a caller about to act on a
    /// user's silence should be asking. See [`Self::is_confirmed_listening`].
    pub fn is_blind(&self) -> bool {
        self.state
            .lock()
            .expect("inbox health mutex poisoned")
            .blind_now()
            .is_some()
    }

    /// Whether an audit has actually confirmed that Mostro can hear.
    ///
    /// This is the predicate for anything that punishes a user for not
    /// answering. It is deliberately false before the first audit: the daemon
    /// subscribes at startup and the watchdog's first pass comes later, so
    /// between the two there is a window in which a node that never obtained a
    /// working inbox would otherwise look healthy and start cancelling orders
    /// and slashing bonds over messages it was never in a position to receive.
    pub fn is_confirmed_listening(&self) -> bool {
        self.state
            .lock()
            .expect("inbox health mutex poisoned")
            .verdict
            == Some(InboxStatus::Listening)
    }

    /// Seconds the inbox was deaf between `from` and now.
    ///
    /// This is the compensation a single order is owed, and it is computed per
    /// order on purpose. A deadline is wall-clock, but the user is answering a
    /// node that has to be listening for the answer to land — and because a
    /// message sent into a dead inbox is lost rather than queued (the
    /// ten-second replay window, see the module docs), they must send it again
    /// once the ear is back. So an order's clock effectively stops for exactly
    /// the outages that overlap its own waiting period: an order that was
    /// already waiting through an outage is owed all of it, one taken
    /// afterwards is owed nothing.
    pub fn blind_seconds_since(&self, from: i64) -> i64 {
        self.blind_seconds_between(from, now_secs())
    }

    fn blind_seconds_between(&self, from: i64, to: i64) -> i64 {
        let state = self.state.lock().expect("inbox health mutex poisoned");
        state.windows.iter().map(|w| w.overlap(from, to, to)).sum()
    }

    /// Upper bound on what any order could be owed.
    ///
    /// No decision is taken on this figure — an order's credit is always the
    /// downtime that overlaps its own wait ([`Self::blind_seconds_since`]).
    /// It exists so the timeout job can tell an operator, in one line, how
    /// much downtime is in play this tick.
    pub fn max_blind_seconds(&self) -> i64 {
        let now = now_secs();
        let state = self.state.lock().expect("inbox health mutex poisoned");
        state
            .windows
            .iter()
            .map(|w| w.end.unwrap_or(now) - w.start)
            .sum::<i64>()
            .max(0)
    }

    /// Record that `relay` answered the inbox REQ (an `EOSE` for our
    /// subscription), which is the only evidence that it is really serving it.
    ///
    /// Whatever was making the relay fail is over, so its pacing is reset too
    /// and the next failure earns a prompt retry again. Returns whether the
    /// relay was being backed off, which is what distinguishes a recovery
    /// worth logging from the steady state.
    pub fn note_relay_acknowledged(&self, relay: &RelayUrl) -> bool {
        self.note_relay_acknowledged_at(relay, now_secs())
    }

    fn note_relay_acknowledged_at(&self, relay: &RelayUrl, at: i64) -> bool {
        let mut state = self.state.lock().expect("inbox health mutex poisoned");
        state.acknowledged.insert(relay.clone(), at);
        state.backoff.remove(relay).is_some()
    }

    /// Whether a re-subscribe to `relay` may go out now, arming the next delay
    /// when it may. The first failure for a relay always passes.
    ///
    /// This is the single pacing budget the event loop and the watchdog share.
    /// Under the watchdog's 30-second cadence the doubling only starts to bite
    /// once the delay outgrows the interval — so a relay that lost the inbox
    /// once is re-subscribed on the very next pass, and only one that keeps
    /// refusing tapers to [`RESUBSCRIBE_MAX_BACKOFF`].
    pub fn allow_resubscribe(&self, relay: &RelayUrl) -> bool {
        self.allow_resubscribe_at(relay, Instant::now())
    }

    fn allow_resubscribe_at(&self, relay: &RelayUrl, now: Instant) -> bool {
        let mut state = self.state.lock().expect("inbox health mutex poisoned");
        match state.backoff.get_mut(relay) {
            None => {
                state.backoff.insert(
                    relay.clone(),
                    RelayBackoff {
                        next_attempt_at: now + RESUBSCRIBE_INITIAL_BACKOFF,
                        delay: RESUBSCRIBE_INITIAL_BACKOFF,
                    },
                );
                true
            }
            Some(pacing) => {
                if now < pacing.next_attempt_at {
                    return false;
                }
                pacing.delay = (pacing.delay * 2).min(RESUBSCRIBE_MAX_BACKOFF);
                pacing.next_attempt_at = now + pacing.delay;
                true
            }
        }
    }

    /// Forget `relay`'s acknowledgement: whatever it said about serving the
    /// inbox no longer applies.
    ///
    /// Two callers, one meaning. A fresh REQ has gone out and has yet to be
    /// answered ([`resubscribe_relay`]); or the relay closed the subscription
    /// provisionally, so the entry the SDK left registered is not evidence of
    /// anything until the replacement REQ is answered ([`InboxKeeper::on_relay_message`]).
    /// In both cases the audit has to judge the relay on the new evidence
    /// rather than on the old `EOSE`.
    pub fn note_relay_unacknowledged(&self, relay: &RelayUrl) {
        self.state
            .lock()
            .expect("inbox health mutex poisoned")
            .acknowledged
            .remove(relay);
    }

    /// Whether `relay` answered the inbox REQ *on its current connection*.
    ///
    /// `connected_at` is when the websocket the audit is looking at was
    /// established. An acknowledgement older than that was earned on a session
    /// that no longer exists: the SDK re-sends the REQ on reconnect of its own
    /// accord (`should_resubscribe`), emitting nothing this module can see, so
    /// a relay that comes back and then quietly ignores the replacement would
    /// otherwise keep reading as healthy on the strength of its old `EOSE`.
    ///
    /// The comparison is inclusive so that an `EOSE` landing in the same
    /// second as the connect still counts.
    pub fn has_acknowledged_since(&self, relay: &RelayUrl, connected_at: i64) -> bool {
        self.state
            .lock()
            .expect("inbox health mutex poisoned")
            .acknowledged
            .get(relay)
            .is_some_and(|&at| at >= connected_at)
    }

    /// How long the current outage has been running, or zero if listening.
    pub fn blind_for_secs(&self) -> i64 {
        let now = now_secs();
        self.state
            .lock()
            .expect("inbox health mutex poisoned")
            .blind_now()
            .map(|w| (now - w.start).max(0))
            .unwrap_or(0)
    }

    /// How long it has been since an audit confirmed Mostro can hear.
    ///
    /// Zero while listening. Otherwise it counts from the start of the current
    /// outage, or — if no audit has ever run — from startup, so a watchdog
    /// that never reported cannot leave a caller waiting forever on a verdict
    /// that is not coming.
    pub fn unconfirmed_for_secs(&self) -> i64 {
        let now = now_secs();
        let state = self.state.lock().expect("inbox health mutex poisoned");
        if state.verdict == Some(InboxStatus::Listening) && state.blind_now().is_none() {
            return 0;
        }
        let since = state
            .blind_now()
            .map(|w| w.start)
            .unwrap_or(state.installed_at);
        (now - since).max(0)
    }
}

/// Wall-clock seconds, the base an order's `taken_at` is recorded in.
fn now_secs() -> i64 {
    Timestamp::now().as_secs() as i64
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
    use super::*;

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

    /// Whether `health` is currently pacing re-subscribes to `url`.
    fn backing_off(health: &InboxHealth, url: &RelayUrl) -> bool {
        health
            .state
            .lock()
            .expect("inbox health mutex poisoned")
            .backoff
            .contains_key(url)
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

    // ───────────────────────────── backoff pacing ─────────────────────────────

    #[test]
    fn first_closure_from_a_relay_retries_immediately() {
        let health = InboxHealth::at(T0);
        let relay = relay_url("ws://relay.example");

        assert!(
            health.allow_resubscribe_at(&relay, Instant::now()),
            "a first CLOSED must be answered at once: every delay is deaf time"
        );
    }

    #[test]
    fn repeat_closures_are_paced_and_back_off() {
        let health = InboxHealth::at(T0);
        let relay = relay_url("ws://relay.example");
        let start = Instant::now();

        assert!(health.allow_resubscribe_at(&relay, start));
        // A relay that closes again right away must not pull a second REQ.
        assert!(!health.allow_resubscribe_at(&relay, start));
        assert!(!health.allow_resubscribe_at(&relay, start + Duration::from_secs(1)));

        // Past the first delay it retries, and the next wait is longer.
        assert!(health.allow_resubscribe_at(&relay, start + RESUBSCRIBE_INITIAL_BACKOFF));
        assert!(!health.allow_resubscribe_at(&relay, start + RESUBSCRIBE_INITIAL_BACKOFF * 2));
        assert!(health.allow_resubscribe_at(&relay, start + RESUBSCRIBE_INITIAL_BACKOFF * 3));
    }

    #[test]
    fn backoff_is_capped() {
        let health = InboxHealth::at(T0);
        let relay = relay_url("ws://relay.example");
        let mut now = Instant::now();

        // Drive it well past the ceiling.
        for _ in 0..20 {
            assert!(health.allow_resubscribe_at(&relay, now));
            now += RESUBSCRIBE_MAX_BACKOFF * 2;
        }

        assert_eq!(
            health
                .state
                .lock()
                .unwrap()
                .backoff
                .get(&relay)
                .expect("state kept")
                .delay,
            RESUBSCRIBE_MAX_BACKOFF,
            "a hostile relay must still be retried every {RESUBSCRIBE_MAX_BACKOFF:?}"
        );
    }

    #[test]
    fn backoff_is_per_relay() {
        let health = InboxHealth::at(T0);
        let hostile = relay_url("ws://hostile.example");
        let healthy = relay_url("ws://healthy.example");
        let now = Instant::now();

        assert!(health.allow_resubscribe_at(&hostile, now));
        assert!(!health.allow_resubscribe_at(&hostile, now));
        // One misbehaving relay must not delay recovery on another.
        assert!(health.allow_resubscribe_at(&healthy, now));
    }

    #[test]
    fn an_acknowledgement_clears_the_pacing_for_the_next_failure() {
        let health = InboxHealth::at(T0);
        let relay = relay_url("ws://relay.example");
        let now = Instant::now();

        assert!(health.allow_resubscribe_at(&relay, now));
        assert!(!health.allow_resubscribe_at(&relay, now));

        assert!(
            health.note_relay_acknowledged(&relay),
            "clearing a live backoff entry is what marks a recovery"
        );
        assert!(
            health.allow_resubscribe_at(&relay, now),
            "a relay that answered starts over: the next failure is a fresh one"
        );

        // The steady state has nothing to clear, so nothing to report either.
        health.note_relay_acknowledged(&relay);
        assert!(!health.note_relay_acknowledged(&relay));
    }

    #[test]
    fn the_watchdog_cadence_recovers_promptly_and_only_then_tapers() {
        // The point of sharing one budget: an audit every
        // `INBOX_WATCHDOG_INTERVAL` must still re-subscribe a relay that
        // simply lost the inbox, while a relay that refuses it converges on
        // the advertised ceiling instead of drawing a REQ every 30 seconds
        // forever.
        let health = InboxHealth::at(T0);
        let relay = relay_url("ws://hostile.example");
        let tick = Duration::from_secs(crate::scheduler::INBOX_WATCHDOG_INTERVAL);
        let mut now = Instant::now();

        assert!(
            health.allow_resubscribe_at(&relay, now),
            "the pass that first notices the loss must act on it"
        );
        for pass in 1..=3 {
            now += tick;
            assert!(
                health.allow_resubscribe_at(&relay, now),
                "pass {pass}: a delay still under the audit interval must not skip a retry"
            );
        }

        // Once the doubling outgrows the interval, passes start being skipped.
        let mut attempts = 0;
        for _ in 0..40 {
            now += tick;
            if health.allow_resubscribe_at(&relay, now) {
                attempts += 1;
            }
        }
        assert!(
            attempts < 40,
            "a relay that keeps refusing must stop drawing a REQ on every pass"
        );
        assert_eq!(
            health
                .state
                .lock()
                .unwrap()
                .backoff
                .get(&relay)
                .expect("state kept")
                .delay,
            RESUBSCRIBE_MAX_BACKOFF
        );
    }

    // ───────────────────────── control-plane handling ─────────────────────────

    #[tokio::test]
    async fn eose_for_the_inbox_clears_the_backoff() {
        let client = crate::util::mostro_nostr_client_options(None).build();
        let (keeper, health) = keeper();
        let relay = relay_url("ws://relay.example");

        assert!(health.allow_resubscribe(&relay));
        assert!(backing_off(&health, &relay));

        let eose = RelayMessage::EndOfStoredEvents(std::borrow::Cow::Owned(
            keeper.subscription.id().clone(),
        ));
        keeper.on_relay_message(&client, &relay, &eose).await;

        assert!(
            !backing_off(&health, &relay),
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
            !backing_off(&health, &relay),
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
            backing_off(&health, &url),
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
            !backing_off(&health, &relay),
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

    // ───────────────────────────── health record ─────────────────────────────

    /// Health observations are wall-clock based, so tests drive a fixed origin
    /// rather than the real clock.
    const T0: i64 = 1_700_000_000;

    #[test]
    fn health_records_an_outage_from_first_blindness_to_recovery() {
        let health = InboxHealth::at(T0);

        assert!(!health.is_blind(), "a fresh record starts out listening");

        health.observe(InboxStatus::Blind, T0);
        assert!(health.is_blind());

        // Staying blind must not restart the clock — the outage began at the
        // first observation, and that is what an order is owed.
        health.observe(InboxStatus::Blind, T0 + 30);
        assert!(health.is_blind());

        health.observe(InboxStatus::Listening, T0 + 90);
        assert!(!health.is_blind());

        assert_eq!(
            health.blind_seconds_between(T0, T0 + 90),
            90,
            "the recorded outage must span the whole blind window"
        );
    }

    #[test]
    fn health_is_not_listening_until_an_audit_says_so() {
        let health = InboxHealth::at(T0);

        // Startup is not evidence. Between `main` subscribing and the
        // watchdog's first pass, a node whose inbox never worked would
        // otherwise process timeouts as if it had been listening all along.
        assert!(
            !health.is_confirmed_listening(),
            "an unaudited record must not authorise acting on a user's silence"
        );
        assert!(
            !health.is_blind(),
            "nor should it claim an outage it has not observed"
        );

        health.observe(InboxStatus::Listening, T0);
        assert!(health.is_confirmed_listening());
    }

    #[test]
    fn a_blind_first_audit_dates_the_outage_from_startup() {
        let health = InboxHealth::at(T0);

        // The watchdog's first pass comes some time after boot. Finding the
        // inbox deaf then means it was deaf for that whole stretch, not just
        // from the moment somebody looked.
        health.observe(InboxStatus::Blind, T0 + 30);
        health.observe(InboxStatus::Listening, T0 + 90);

        assert_eq!(
            health.blind_seconds_between(T0, T0 + 90),
            90,
            "the outage must be dated from startup, not from the first audit"
        );
    }

    #[test]
    fn a_node_that_was_never_blind_owes_nothing() {
        let health = InboxHealth::at(T0);
        health.observe(InboxStatus::Listening, T0);

        assert_eq!(health.blind_seconds_between(T0, T0 + 10_000), 0);
        assert_eq!(health.max_blind_seconds(), 0);
    }

    // ──────────────────── what a single order is owed ────────────────────

    #[test]
    fn an_order_is_owed_only_the_downtime_it_waited_through() {
        let health = InboxHealth::at(T0);
        // One outage: [T0+100, T0+400], five minutes.
        health.observe(InboxStatus::Listening, T0);
        health.observe(InboxStatus::Blind, T0 + 100);
        health.observe(InboxStatus::Listening, T0 + 400);

        let now = T0 + 1_000;

        // Waiting since before it started: owed the whole outage.
        assert_eq!(health.blind_seconds_between(T0, now), 300);
        // Taken midway through: owed only the remainder.
        assert_eq!(health.blind_seconds_between(T0 + 250, now), 150);
        // Taken after it ended: owed nothing. This is what a single global
        // allowance got wrong — it credited orders that never lost a second.
        assert_eq!(health.blind_seconds_between(T0 + 500, now), 0);
    }

    #[test]
    fn compensation_does_not_evaporate_as_time_passes() {
        let health = InboxHealth::at(T0);
        health.observe(InboxStatus::Listening, T0);
        health.observe(InboxStatus::Blind, T0 + 100);
        health.observe(InboxStatus::Listening, T0 + 400);

        // The debt an order carries is a property of when it waited, not of
        // how long ago the outage was. A decaying allowance wore off at the
        // same rate the deadline advanced, so it compensated almost nothing.
        for probe in [400, 700, 5_000, 50_000] {
            assert_eq!(
                health.blind_seconds_between(T0, T0 + probe),
                300,
                "an order waiting since T0 is owed the outage regardless of when we ask"
            );
        }
    }

    #[test]
    fn an_order_waiting_through_an_outage_survives_its_nominal_deadline() {
        // The regression in full: 900s timeout, an order taken at T0, and a
        // 300s outage right at the start. Under the old decaying allowance
        // this order was cancelled at ~T0+900, having had only 600s of
        // listening time.
        let health = InboxHealth::at(T0);
        health.observe(InboxStatus::Blind, T0);
        health.observe(InboxStatus::Listening, T0 + 300);

        let exp_seconds = 900i64;
        let late_at = |now: i64| {
            let owed = health.blind_seconds_between(T0, now);
            (now - T0) >= exp_seconds + owed
        };

        assert!(!late_at(T0 + 900), "cancelled after only 600s of listening");
        assert!(!late_at(T0 + 1_199));
        assert!(
            late_at(T0 + 1_200),
            "and it must still expire once it has had its full 900s"
        );
    }

    /// Regression: the credit is per order, so an order taken *after* an
    /// outage ended must expire at its nominal deadline.
    ///
    /// The timeout job used to widen `find_order_by_seconds` by
    /// [`InboxHealth::max_blind_seconds`], which narrows the selection rather
    /// than widening it — every surviving row was already past
    /// `deadline + max_blind_seconds`, the per-order check could never spare
    /// anything, and what shipped was the global allowance this design
    /// rejects. That allowance grows with every outage in the retention
    /// window, so a node with flapping relays would postpone every deadline by
    /// hours of unrelated downtime.
    #[test]
    fn an_order_taken_after_an_outage_is_not_credited_for_it() {
        let health = InboxHealth::at(T0);
        // One outage: [T0, T0+300].
        health.observe(InboxStatus::Blind, T0);
        health.observe(InboxStatus::Listening, T0 + 300);

        let exp_seconds = 900i64;
        let late_at = |taken_at: i64, now: i64| {
            let owed = health.blind_seconds_between(taken_at, now);
            (now - taken_at) >= exp_seconds + owed
        };

        // A: waited through the whole outage, owed all 300s.
        assert!(!late_at(T0, T0 + 1_199));
        assert!(late_at(T0, T0 + 1_200));

        // B: taken after recovery, owed nothing — even though the node's total
        // downtime is the same 300s the global allowance would have handed it.
        assert_eq!(health.max_blind_seconds(), 300);
        assert!(!late_at(T0 + 400, T0 + 1_299));
        assert!(
            late_at(T0 + 400, T0 + 1_300),
            "an order that never lost a second must expire at its nominal deadline"
        );
    }

    #[test]
    fn consecutive_outages_accumulate_their_debt() {
        let health = InboxHealth::at(T0);
        health.observe(InboxStatus::Listening, T0);
        health.observe(InboxStatus::Blind, T0 + 100);
        health.observe(InboxStatus::Listening, T0 + 200);
        health.observe(InboxStatus::Blind, T0 + 240);
        health.observe(InboxStatus::Listening, T0 + 290);

        assert_eq!(
            health.blind_seconds_between(T0, T0 + 1_000),
            150,
            "an order waiting through both outages is owed both"
        );
        assert_eq!(
            health.blind_seconds_between(T0 + 210, T0 + 1_000),
            50,
            "one taken between them is owed only the second"
        );
    }

    #[test]
    fn an_ongoing_outage_counts_up_to_now() {
        let health = InboxHealth::at(T0);
        health.observe(InboxStatus::Listening, T0);
        health.observe(InboxStatus::Blind, T0 + 100);

        assert_eq!(health.blind_seconds_between(T0, T0 + 400), 300);
        assert_eq!(health.blind_seconds_between(T0, T0 + 900), 800);
    }

    #[test]
    fn stale_windows_are_pruned() {
        let health = InboxHealth::at(T0);
        health.observe(InboxStatus::Listening, T0);
        health.observe(InboxStatus::Blind, T0 + 100);
        health.observe(InboxStatus::Listening, T0 + 200);

        // Far past the retention horizon, the old window is dropped rather
        // than accumulating for the life of the process.
        let much_later = T0 + BLIND_WINDOW_RETENTION_SECS + 1_000;
        health.observe(InboxStatus::Listening, much_later);

        assert_eq!(health.blind_seconds_between(T0, much_later), 0);
        assert!(health.state.lock().expect("lock").windows.is_empty());
    }

    #[test]
    fn unconfirmed_time_counts_from_the_outage_or_from_startup() {
        // What bounds how long the timeout job may defer. It has to answer
        // even when no audit ever ran, or a watchdog that died would park the
        // job on a verdict that is never coming.
        let never_audited = InboxHealth::at(now_secs() - 120);
        assert!(
            never_audited.unconfirmed_for_secs() >= 120,
            "with no verdict at all, the clock runs from startup"
        );

        let healthy = InboxHealth::at(now_secs());
        healthy.observe(InboxStatus::Listening, now_secs());
        assert_eq!(
            healthy.unconfirmed_for_secs(),
            0,
            "a confirmed inbox owes no waiting"
        );

        let blind = InboxHealth::at(now_secs() - 600);
        blind.observe(InboxStatus::Listening, now_secs() - 600);
        blind.observe(InboxStatus::Blind, now_secs() - 300);
        assert!(
            (300..=310).contains(&blind.unconfirmed_for_secs()),
            "while blind it runs from the start of the outage, got {}",
            blind.unconfirmed_for_secs()
        );
    }

    #[test]
    fn health_ignores_repeated_healthy_observations() {
        let health = InboxHealth::at(T0);

        health.observe(InboxStatus::Listening, T0);
        health.observe(InboxStatus::Listening, T0 + 30);

        assert!(!health.is_blind());
        assert_eq!(
            health.blind_seconds_between(T0, T0 + 30),
            0,
            "a node that was never blind has no outage to compensate for"
        );
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

    #[test]
    fn an_acknowledgement_does_not_survive_the_connection_it_was_earned_on() {
        // A websocket drop and reconnect leaves no trace the keeper can act
        // on: there is no relay-status `ClientNotification` in nostr-sdk
        // 0.45.1, and the SDK silently re-sends the REQ by itself
        // (`should_resubscribe`). If the relay then ignores that replacement,
        // the only thing standing between a deaf node and resumed slashing is
        // the acknowledgement expiring with its session.
        let health = InboxHealth::at(T0);
        let url = relay_url("ws://relay.example");

        health.note_relay_acknowledged_at(&url, T0 + 100);

        assert!(health.has_acknowledged_since(&url, T0 + 50));
        assert!(
            health.has_acknowledged_since(&url, T0 + 100),
            "an EOSE landing in the same second as the connect must still count"
        );
        assert!(
            !health.has_acknowledged_since(&url, T0 + 101),
            "credit earned on a previous connection must not vouch for this one"
        );
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
            backing_off(&health, &url),
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
