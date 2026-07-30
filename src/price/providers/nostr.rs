//! Nostr trusted-node relay-mode quoter (spec §11.7).
//!
//! Instead of an HTTP API, this provider sources BTC/fiat quotes from the
//! same kind-30078 NIP-33 rate event Mostro nodes already publish
//! (`nip33::new_exchange_rates_event`, `docs/NOSTR_EXCHANGE_RATES.md`) — for
//! operators in regions where price APIs are DNS/IP blocked but Nostr relays
//! are reachable (issue #697). It queries the process-wide Nostr client
//! (already connected to the `[nostr]` relays) for the latest such event
//! from each configured `trusted_nodes` pubkey and takes the **freshest**
//! one as this tick's source — no cross-node statistical combine; a stale or
//! unreachable trusted node is simply outrun by a fresher one. Events whose
//! `created_at` is older than `[price].max_price_staleness_seconds` are
//! discarded so a relay that still serves an expired kind-30078 cannot keep
//! refreshing the local cache clock with zombie rates.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use nostr_sdk::prelude::*;
use serde::Deserialize;

use crate::config::constants::NOSTR_EXCHANGE_RATES_EVENT_KIND;
use crate::price::config::ProviderConfig;
use crate::price::provider::{PriceProvider, ProviderError, ProviderId, ProviderQuotes, Quote};

/// Same wrapper the daemon publishes: `{"BTC": {ccy: price}}`.
#[derive(Debug, Deserialize)]
struct RatesContent {
    #[serde(rename = "BTC")]
    btc: HashMap<String, f64>,
}

/// Trusted-node Nostr quoter.
pub struct NostrProvider {
    trusted_nodes: Vec<PublicKey>,
    /// One-shot relay query bound, derived from the shared
    /// `provider_timeout_seconds` (see `new`) rather than a fixed constant —
    /// an operator who lowers that setting must not have this provider's
    /// queries consistently swallowed by `PriceManager::poll_budget`'s outer
    /// timeout before they can complete or fail on their own (CodeRabbit,
    /// PR #841).
    query_timeout: Duration,
    /// Maximum age of a trusted-node rate event (`created_at`), taken from
    /// the shared `[price].max_price_staleness_seconds` so upstream Nostr
    /// freshness uses the same TTL the store enforces on cached quotes.
    max_age: Duration,
}

impl NostrProvider {
    /// Build the provider from its `[price.providers.nostr]` sub-table.
    ///
    /// Fails fast (mirrors `ElToqueProvider::new`'s missing-token check) when
    /// `trusted_nodes` is empty or contains a pubkey that isn't valid hex —
    /// `ProviderConfig::validate` only checks the list is non-empty, not
    /// that its entries parse (spec §7).
    ///
    /// `provider_timeout_seconds` is the shared `[price]` setting (not
    /// per-provider config): `query_timeout` is set to exactly that value,
    /// so it fits under `poll_budget`'s `provider_timeout_seconds + 1s`
    /// (no `fallback_urls` for this provider ⇒ one attempt) with the same
    /// 1s of slack every other provider gets from the shared `reqwest`
    /// client's own per-attempt timeout.
    ///
    /// `max_price_staleness_seconds` is the same shared TTL used by
    /// [`crate::price::store::PriceStore`]: events older than that are not
    /// eligible as this tick's source.
    pub fn new(
        cfg: &ProviderConfig,
        provider_timeout_seconds: u64,
        max_price_staleness_seconds: i64,
    ) -> Result<Self, String> {
        if cfg.trusted_nodes.is_empty() {
            return Err(
                "price provider 'nostr': enabled provider requires at least one \
                 `trusted_nodes` pubkey (see docs/PRICE_PROVIDERS.md §11.7)"
                    .to_string(),
            );
        }
        let trusted_nodes = cfg
            .trusted_nodes
            .iter()
            .map(|hex| {
                PublicKey::from_hex(hex).map_err(|e| {
                    format!(
                        "price provider 'nostr': invalid `trusted_nodes` pubkey \
                         '{hex}': {e}"
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            trusted_nodes,
            query_timeout: Duration::from_secs(provider_timeout_seconds.max(1)),
            // `PriceSettings::validate` already rejects non-positive values;
            // clamp here so a zero can never make every event look fresh.
            max_age: Duration::from_secs(max_price_staleness_seconds.max(1) as u64),
        })
    }

    /// The relay query for this tick: the latest `mostro-rates` (kind
    /// 30078) event from any configured trusted node. Split out from
    /// [`PriceProvider::fetch`] so its shape is unit-testable without a
    /// relay (spec §10.5) — a wrong `kind`/`identifier` here would
    /// otherwise only surface via the `#[ignore]`d live-relay test.
    pub(crate) fn build_filter(&self) -> Filter {
        Filter::new()
            .kind(Kind::Custom(NOSTR_EXCHANGE_RATES_EVENT_KIND))
            .authors(self.trusted_nodes.clone())
            .identifier("mostro-rates")
    }

    /// Parse a rate event's `content` into [`ProviderQuotes`]. Split out so
    /// it is unit-testable without a relay (spec §10.5).
    pub(crate) fn parse_content(body: &str) -> Result<ProviderQuotes, ProviderError> {
        let parsed: RatesContent =
            serde_json::from_str(body).map_err(|e| ProviderError::Parse(format!("nostr: {e}")))?;
        Ok(parsed
            .btc
            .into_iter()
            .filter_map(|(code, v)| match v {
                v if v.is_finite() && v > 0.0 => Some((code.to_uppercase(), Quote::PerBtc(v))),
                _ => None,
            })
            .collect())
    }

    /// Among `events`, the freshest one authored by a trusted pubkey whose
    /// `created_at` is within `max_age` of `now` — a second client-side trust
    /// check even though the relay-side `authors` filter should already
    /// guarantee the pubkey (spec §10.3 / `NOSTR_EXCHANGE_RATES.md`
    /// "clients MUST verify pubkey"), plus a freshness gate so relays that
    /// ignore NIP-40 expiration cannot serve zombie rates. The event's own
    /// NIP-40 `expiration` tag, if any, is also honored directly via
    /// `Event::is_expired_at` — the `max_age` gate alone only bounds
    /// staleness by this node's clock, so a trusted node advertising a
    /// shorter self-declared expiry is still respected. Pure and
    /// unit-testable without a relay.
    pub(crate) fn select_freshest<'a>(
        events: &'a [Event],
        trusted: &[PublicKey],
        now: Timestamp,
        max_age: Duration,
    ) -> Option<&'a Event> {
        let max_age_secs = max_age.as_secs();
        events
            .iter()
            .filter(|e| trusted.contains(&e.pubkey))
            // A future-dated `created_at` (forged or clock-skewed relay) would
            // otherwise saturate the age check to 0 and win `max_by_key` outright.
            .filter(|e| e.created_at <= now)
            .filter(|e| now.as_secs().saturating_sub(e.created_at.as_secs()) <= max_age_secs)
            .filter(|e| !e.is_expired_at(&now))
            .max_by_key(|e| e.created_at)
    }
}

#[async_trait]
impl PriceProvider for NostrProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Nostr
    }

    async fn fetch(&self, _http: &reqwest::Client) -> Result<ProviderQuotes, ProviderError> {
        let client = crate::util::get_nostr_client()
            .map_err(|e| ProviderError::Http(format!("nostr: {e}")))?;

        let events = client
            .fetch_events(self.build_filter(), self.query_timeout)
            .await
            .map_err(|e| ProviderError::Http(format!("nostr: relay query failed: {e}")))?
            .into_iter()
            .collect::<Vec<Event>>();

        let event =
            Self::select_freshest(&events, &self.trusted_nodes, Timestamp::now(), self.max_age)
                .ok_or_else(|| {
                    ProviderError::Http(
                        "nostr: no fresh trusted-node rate event found \
                 (all missing, untrusted, or older than max_price_staleness_seconds)"
                            .to_string(),
                    )
                })?;

        Self::parse_content(&event.content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CONTENT: &str = r#"{"BTC": {"USD": 50000.0, "eur": 45000.0, "ARS": 105000000.0}}"#;

    #[test]
    fn parse_content_upper_cases_codes() {
        let quotes = NostrProvider::parse_content(SAMPLE_CONTENT).unwrap();
        assert_eq!(quotes.get("USD"), Some(&Quote::PerBtc(50_000.0)));
        assert_eq!(quotes.get("EUR"), Some(&Quote::PerBtc(45_000.0)));
        assert_eq!(quotes.get("ARS"), Some(&Quote::PerBtc(105_000_000.0)));
    }

    #[test]
    fn parse_content_drops_non_finite_and_non_positive() {
        let body = r#"{"BTC": {"USD": 0, "EUR": -1, "GBP": 50000.0}}"#;
        let quotes = NostrProvider::parse_content(body).unwrap();
        assert_eq!(quotes.len(), 1, "only GBP is a usable rate");
        assert_eq!(quotes.get("GBP"), Some(&Quote::PerBtc(50_000.0)));
    }

    #[test]
    fn parse_content_error_is_returned() {
        let err = NostrProvider::parse_content("not json").unwrap_err();
        assert!(matches!(err, ProviderError::Parse(_)));
    }

    fn signed_event(keys: &Keys, content: &str, created_at: u64) -> Event {
        EventBuilder::new(Kind::Custom(NOSTR_EXCHANGE_RATES_EVENT_KIND), content)
            .tags(vec![Tag::identifier("mostro-rates")])
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .expect("event must sign")
    }

    /// Like `signed_event`, but carrying a NIP-40 `expiration` tag — for
    /// exercising `select_freshest`'s `is_expired_at` gate independently of
    /// the `max_age` gate.
    fn signed_event_expiring_at(
        keys: &Keys,
        content: &str,
        created_at: u64,
        expiration: u64,
    ) -> Event {
        EventBuilder::new(Kind::Custom(NOSTR_EXCHANGE_RATES_EVENT_KIND), content)
            .tags(vec![
                Tag::identifier("mostro-rates"),
                Tag::expiration(Timestamp::from(expiration)),
            ])
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .expect("event must sign")
    }

    /// Relative clock for `select_freshest` unit tests — events use small
    /// synthetic `created_at` values, so tests pass an explicit `now`
    /// instead of wall-clock time.
    const NOW: u64 = 10_000;
    const MAX_AGE: Duration = Duration::from_secs(5_000);

    #[test]
    fn select_freshest_returns_none_for_empty_events() {
        let trusted = vec![Keys::generate().public_key()];
        assert!(
            NostrProvider::select_freshest(&[], &trusted, Timestamp::from(NOW), MAX_AGE).is_none()
        );
    }

    #[test]
    fn select_freshest_ignores_untrusted_authors() {
        let trusted_keys = Keys::generate();
        let untrusted_keys = Keys::generate();
        let trusted = vec![trusted_keys.public_key()];

        let untrusted_event = signed_event(&untrusted_keys, SAMPLE_CONTENT, NOW - 100);
        let events = vec![untrusted_event];

        assert!(
            NostrProvider::select_freshest(&events, &trusted, Timestamp::from(NOW), MAX_AGE)
                .is_none()
        );
    }

    #[test]
    fn select_freshest_picks_the_newest_trusted_event() {
        let older_keys = Keys::generate();
        let newer_keys = Keys::generate();
        let trusted = vec![older_keys.public_key(), newer_keys.public_key()];

        let older = signed_event(&older_keys, SAMPLE_CONTENT, NOW - 2_000);
        let newer = signed_event(&newer_keys, SAMPLE_CONTENT, NOW - 100);
        let events = vec![older, newer.clone()];

        let picked =
            NostrProvider::select_freshest(&events, &trusted, Timestamp::from(NOW), MAX_AGE)
                .unwrap();
        assert_eq!(picked.id, newer.id);
    }

    #[test]
    fn select_freshest_discards_events_older_than_max_age() {
        let keys = Keys::generate();
        let trusted = vec![keys.public_key()];

        let stale = signed_event(&keys, SAMPLE_CONTENT, NOW - MAX_AGE.as_secs() - 1);
        let fresh = signed_event(&keys, SAMPLE_CONTENT, NOW - 100);
        let events = vec![stale, fresh.clone()];

        let picked =
            NostrProvider::select_freshest(&events, &trusted, Timestamp::from(NOW), MAX_AGE)
                .unwrap();
        assert_eq!(picked.id, fresh.id);
    }

    #[test]
    fn select_freshest_returns_none_when_all_trusted_are_stale() {
        let keys = Keys::generate();
        let trusted = vec![keys.public_key()];

        let stale = signed_event(&keys, SAMPLE_CONTENT, NOW - MAX_AGE.as_secs() - 1);
        let events = vec![stale];

        assert!(
            NostrProvider::select_freshest(&events, &trusted, Timestamp::from(NOW), MAX_AGE)
                .is_none()
        );
    }

    #[test]
    fn select_freshest_keeps_event_exactly_at_max_age_boundary() {
        let keys = Keys::generate();
        let trusted = vec![keys.public_key()];

        let at_boundary = signed_event(&keys, SAMPLE_CONTENT, NOW - MAX_AGE.as_secs());
        let events = vec![at_boundary.clone()];

        let picked =
            NostrProvider::select_freshest(&events, &trusted, Timestamp::from(NOW), MAX_AGE)
                .unwrap();
        assert_eq!(picked.id, at_boundary.id);
    }

    #[test]
    fn select_freshest_rejects_future_dated_events() {
        let keys = Keys::generate();
        let trusted = vec![keys.public_key()];

        // A forged or clock-skewed `created_at` in the future must not win
        // `max_by_key` over a legitimately current event.
        let future = signed_event(&keys, SAMPLE_CONTENT, NOW + 1_000);
        let current = signed_event(&keys, SAMPLE_CONTENT, NOW - 100);
        let events = vec![future, current.clone()];

        let picked =
            NostrProvider::select_freshest(&events, &trusted, Timestamp::from(NOW), MAX_AGE)
                .unwrap();
        assert_eq!(picked.id, current.id);
    }

    #[test]
    fn select_freshest_rejects_an_expired_event_even_if_it_is_the_newest() {
        let keys = Keys::generate();
        let trusted = vec![keys.public_key()];

        // Newer by created_at, but self-declared expired via NIP-40 — a
        // relay that ignores expiration and still serves it must not let it
        // win over an older, still-valid event.
        let expired = signed_event_expiring_at(&keys, SAMPLE_CONTENT, NOW - 100, NOW - 50);
        let valid = signed_event(&keys, SAMPLE_CONTENT, NOW - 2_000);
        let events = vec![expired, valid.clone()];

        let picked =
            NostrProvider::select_freshest(&events, &trusted, Timestamp::from(NOW), MAX_AGE)
                .unwrap();
        assert_eq!(picked.id, valid.id);
    }

    #[test]
    fn select_freshest_returns_none_when_every_trusted_event_is_expired() {
        let keys = Keys::generate();
        let trusted = vec![keys.public_key()];

        let expired = signed_event_expiring_at(&keys, SAMPLE_CONTENT, NOW - 100, NOW - 50);
        let events = vec![expired];

        assert!(
            NostrProvider::select_freshest(&events, &trusted, Timestamp::from(NOW), MAX_AGE)
                .is_none()
        );
    }

    fn sample_cfg(trusted_hex: String) -> ProviderConfig {
        ProviderConfig {
            enabled: true,
            url: String::new(),
            fallback_urls: vec![],
            api_key: None,
            token: None,
            only: None,
            except: None,
            trusted_nodes: vec![trusted_hex],
        }
    }

    #[test]
    fn new_parses_valid_hex_trusted_nodes() {
        let cfg = sample_cfg(Keys::generate().public_key().to_hex());
        assert!(NostrProvider::new(&cfg, 10, 1_800).is_ok());
    }

    #[test]
    fn new_derives_query_timeout_and_max_age_from_shared_settings() {
        let cfg = sample_cfg(Keys::generate().public_key().to_hex());
        let provider = NostrProvider::new(&cfg, 7, 1_800).unwrap();
        assert_eq!(provider.query_timeout, Duration::from_secs(7));
        assert_eq!(provider.max_age, Duration::from_secs(1_800));

        // A misconfigured 0 must not produce a zero-duration timeout/age.
        let provider = NostrProvider::new(&cfg, 0, 0).unwrap();
        assert_eq!(provider.query_timeout, Duration::from_secs(1));
        assert_eq!(provider.max_age, Duration::from_secs(1));
    }

    #[test]
    fn build_filter_has_kind_authors_and_identifier() {
        let node_a = Keys::generate().public_key();
        let node_b = Keys::generate().public_key();
        let cfg = ProviderConfig {
            enabled: true,
            url: String::new(),
            fallback_urls: vec![],
            api_key: None,
            token: None,
            only: None,
            except: None,
            trusted_nodes: vec![node_a.to_hex(), node_b.to_hex()],
        };
        let provider = NostrProvider::new(&cfg, 10, 1_800).unwrap();

        let expected = Filter::new()
            .kind(Kind::Custom(NOSTR_EXCHANGE_RATES_EVENT_KIND))
            .authors([node_a, node_b])
            .identifier("mostro-rates");
        assert_eq!(provider.build_filter(), expected);
    }

    #[test]
    fn new_rejects_empty_trusted_nodes() {
        let cfg = ProviderConfig {
            enabled: true,
            url: String::new(),
            fallback_urls: vec![],
            api_key: None,
            token: None,
            only: None,
            except: None,
            trusted_nodes: vec![],
        };
        assert!(NostrProvider::new(&cfg, 10, 1_800).is_err());
    }

    #[test]
    fn new_rejects_invalid_hex_pubkey() {
        let cfg = sample_cfg("not-a-pubkey".to_string());
        assert!(NostrProvider::new(&cfg, 10, 1_800).is_err());
    }

    /// Live-relay evidence for issue #697: exercises the real `fetch()` path
    /// (not a fixture) against `wss://relay.mostro.network` and the two
    /// example `trusted_nodes` pubkeys the issue names. Inherently flaky
    /// against third-party infra outside this repo's control, so it's
    /// `#[ignore]`d (never runs in CI) — run explicitly, alone, so no other
    /// test races it to set the process-wide `NOSTR_CLIENT`:
    ///
    /// `cargo test price::providers::nostr::tests::live_relay_fetch_returns_real_rates -- --ignored --exact`
    #[tokio::test]
    #[ignore = "hits a real Nostr relay; run explicitly for manual verification"]
    async fn live_relay_fetch_returns_real_rates() {
        let client = Client::default();
        client
            .add_relay("wss://relay.mostro.network")
            .await
            .expect("add_relay");
        client.connect().await;
        crate::NOSTR_CLIENT
            .set(client)
            .expect("NOSTR_CLIENT must be unset — run this test alone (see doc comment)");

        // Both pubkeys issue #697 names as example trusted_nodes.
        let cfg = ProviderConfig {
            enabled: true,
            url: String::new(),
            fallback_urls: vec![],
            api_key: None,
            token: None,
            only: None,
            except: None,
            trusted_nodes: vec![
                "82fa8cb978b43c79b2156585bac2c011176a21d2aead6d9f7c575c005be88390".to_string(),
                "00000235a3e904cfe1213a8a54d6f1ec1bef7cc6bfaabd6193e82931ccf1366a".to_string(),
            ],
        };
        let provider = NostrProvider::new(&cfg, 10, 1_800).expect("valid hex pubkeys");
        let http = reqwest::Client::new();

        let quotes = provider.fetch(&http).await.expect("live relay fetch");
        println!(
            "nostr provider live fetch: {} currencies — {quotes:?}",
            quotes.len()
        );
        assert!(
            !quotes.is_empty(),
            "expected at least one live currency quote"
        );
    }
}
