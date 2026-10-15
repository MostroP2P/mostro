//! Building the health event (kind 38389, protocol `health_event.md`) and
//! the state that must survive a restart: `seq`, the last `created_at`, and
//! the last relay outage.

use crate::app::daemon_state;
use crate::app::health::check::{CheckName, CheckState, HealthStatus};
use mostro_core::error::MostroError::{self, MostroInternalErr};
use mostro_core::error::ServiceError;
use nostr_sdk::prelude::*;
use sqlx::SqlitePool;

/// Kind of the health event. Defined here until it moves to mostro-core
/// next to the other Mostro kinds.
pub const NOSTR_HEALTH_EVENT_KIND: u16 = 38389;

/// Readers reject an event whose `created_at` is further ahead of their
/// clock than this; the publisher logs an error when it gets there.
pub const MAX_FUTURE_SKEW_SECS: i64 = 300;

/// `daemon_state` keys.
pub const KEY_SEQ: &str = "health_seq";
pub const KEY_CREATED_AT: &str = "health_created_at";
pub const KEY_RELAY_OUTAGE_STARTED_AT: &str = "health_relay_outage_started_at";
pub const KEY_RELAY_OUTAGE_ENDED_AT: &str = "health_relay_outage_ended_at";

/// The LND tags, present together or not at all: only on a Lightning
/// instance whose last `GetInfo` succeeded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LndTags {
    pub block_height: u32,
    pub synced_to_chain: bool,
    pub network: String,
}

/// Everything the public tags say, apart from `seq` and `created_at`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealthReport {
    pub status: HealthStatus,
    /// One entry per check, with the time it entered its state.
    pub checks: Vec<(CheckName, CheckState, i64)>,
    /// `lightning` or `cashu`.
    pub escrow_mode: &'static str,
    pub lnd: Option<LndTags>,
    pub maintenance: bool,
    pub instance_name: Option<String>,
    pub interval: u64,
}

/// `seq` and `created_at` of one publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub seq: u64,
    pub created_at: i64,
}

/// The stamp for the next publication: `seq` one above the last, and
/// `created_at` the clock, but never lower than the last `created_at` plus
/// one, so relays (which keep the newest revision by `created_at`) never
/// prefer an older revision after the clock steps back. The flag is set when
/// that rule put `created_at` more than [`MAX_FUTURE_SKEW_SECS`] ahead of
/// the clock, which readers will reject until the clock catches up.
pub fn next_stamp(last: Option<Stamp>, now: i64) -> (Stamp, bool) {
    let stamp = match last {
        None => Stamp {
            seq: 1,
            created_at: now,
        },
        Some(last) => Stamp {
            seq: last.seq.saturating_add(1),
            created_at: now.max(last.created_at.saturating_add(1)),
        },
    };
    let ahead = stamp.created_at.saturating_sub(now) > MAX_FUTURE_SKEW_SECS;
    (stamp, ahead)
}

/// The public tags, in the order of the protocol's example. Each tag is
/// built once from a field, and the checks come from distinct names, so no
/// tag can repeat (the uniqueness rule).
pub fn health_tags(
    publisher: &PublicKey,
    report: &HealthReport,
    stamp: Stamp,
    recipient: Option<&PublicKey>,
) -> Vec<Tag> {
    let mut tags = vec![
        Tag::identifier(publisher.to_hex()),
        Tag::custom(
            "y",
            crate::nip33::create_platform_tag_values(report.instance_name.as_deref()),
        ),
        Tag::custom("z", ["health"]),
        Tag::custom("status", [report.status.as_str()]),
    ];
    let mut seen = std::collections::BTreeSet::new();
    for (name, state, since) in &report.checks {
        if !seen.insert(*name) {
            continue;
        }
        // `since` is never later than `created_at`.
        let since = (*since).min(stamp.created_at);
        tags.push(Tag::custom(
            "check",
            [
                name.as_str().to_string(),
                state.as_str().to_string(),
                since.to_string(),
            ],
        ));
    }
    tags.push(Tag::custom("escrow_mode", [report.escrow_mode]));
    if let Some(lnd) = &report.lnd {
        tags.push(Tag::custom(
            "lnd_block_height",
            [lnd.block_height.to_string()],
        ));
        tags.push(Tag::custom(
            "lnd_synced_to_chain",
            [lnd.synced_to_chain.to_string()],
        ));
        tags.push(Tag::custom("lnd_network", [lnd.network.clone()]));
    }
    tags.push(Tag::custom(
        "maintenance_mode",
        [report.maintenance.to_string()],
    ));
    tags.push(Tag::custom("mostro_version", [env!("CARGO_PKG_VERSION")]));
    tags.push(Tag::custom("interval", [report.interval.to_string()]));
    tags.push(Tag::custom("seq", [stamp.seq.to_string()]));
    if let Some(recipient) = recipient {
        tags.push(Tag::public_key(*recipient));
    }
    tags.push(Tag::expiration(Timestamp::from(
        expiration(stamp.created_at, report.interval) as u64,
    )));
    tags
}

/// NIP-40 expiration: exactly three intervals after `created_at`.
pub fn expiration(created_at: i64, interval: u64) -> i64 {
    created_at.saturating_add(3 * interval as i64)
}

/// Encrypt the JSON payload to `recipient` (NIP-44 v2) and sign the event.
/// Without a recipient, `content` is empty and there is no `p` tag.
pub fn build_health_event(
    keys: &Keys,
    report: &HealthReport,
    stamp: Stamp,
    payload: &str,
    recipient: Option<&PublicKey>,
) -> Result<Event, MostroError> {
    let content = match recipient {
        Some(recipient) => {
            nip44::encrypt(keys.secret_key(), recipient, payload, nip44::Version::V2)
                .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?
        }
        None => String::new(),
    };
    let tags = health_tags(&keys.public_key(), report, stamp, recipient);
    EventBuilder::new(Kind::Custom(NOSTR_HEALTH_EVENT_KIND), content)
        .tags(tags)
        .custom_created_at(Timestamp::from(stamp.created_at.max(0) as u64))
        .finalize(keys)
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))
}

/// The last relay outage: when the daemon lost its last connected relay,
/// and when one connected again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayOutage {
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
}

impl RelayOutage {
    fn is_active(&self) -> bool {
        self.started_at.is_some() && self.ended_at.is_none()
    }

    /// The outage after observing `connected` relays at `now`, or `None` if
    /// nothing changed. `had_connection` says whether this process has seen
    /// a connected relay before: relays still connecting at boot are not an
    /// outage, while an outage that was going on when the daemon restarted is
    /// closed by the first connection.
    pub fn observe(&self, had_connection: bool, connected: usize, now: i64) -> Option<Self> {
        if connected == 0 && had_connection && !self.is_active() {
            return Some(Self {
                started_at: Some(now),
                ended_at: None,
            });
        }
        if connected > 0 && self.is_active() {
            return Some(Self {
                started_at: self.started_at,
                ended_at: Some(now),
            });
        }
        None
    }
}

/// What the publisher persists between restarts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PersistedHealth {
    pub last_stamp: Option<Stamp>,
    pub relay_outage: RelayOutage,
}

async fn read_i64(pool: &SqlitePool, key: &str) -> Result<Option<i64>, MostroError> {
    Ok(daemon_state::get(pool, key)
        .await?
        .and_then(|value| value.parse::<i64>().ok()))
}

/// Load the persisted state. A missing or unreadable key counts as never
/// written.
pub async fn load_persisted(pool: &SqlitePool) -> Result<PersistedHealth, MostroError> {
    let seq = read_i64(pool, KEY_SEQ).await?;
    let created_at = read_i64(pool, KEY_CREATED_AT).await?;
    let last_stamp = match (seq, created_at) {
        (Some(seq), Some(created_at)) if seq > 0 => Some(Stamp {
            seq: seq as u64,
            created_at,
        }),
        _ => None,
    };
    Ok(PersistedHealth {
        last_stamp,
        relay_outage: RelayOutage {
            started_at: read_i64(pool, KEY_RELAY_OUTAGE_STARTED_AT).await?,
            ended_at: read_i64(pool, KEY_RELAY_OUTAGE_ENDED_AT).await?,
        },
    })
}

fn optional(value: Option<i64>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

async fn commit(tx: sqlx::Transaction<'_, sqlx::Sqlite>) -> Result<(), MostroError> {
    tx.commit()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

/// Persist `seq` and `created_at` together, before the event goes out.
pub async fn save_stamp(pool: &SqlitePool, stamp: Stamp) -> Result<(), MostroError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    daemon_state::set_in(&mut tx, KEY_SEQ, &stamp.seq.to_string()).await?;
    daemon_state::set_in(&mut tx, KEY_CREATED_AT, &stamp.created_at.to_string()).await?;
    commit(tx).await
}

/// Persist the relay outage timestamps together.
pub async fn save_relay_outage(pool: &SqlitePool, outage: RelayOutage) -> Result<(), MostroError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    daemon_state::set_in(
        &mut tx,
        KEY_RELAY_OUTAGE_STARTED_AT,
        &optional(outage.started_at),
    )
    .await?;
    daemon_state::set_in(
        &mut tx,
        KEY_RELAY_OUTAGE_ENDED_AT,
        &optional(outage.ended_at),
    )
    .await?;
    commit(tx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const NOW: i64 = 1_792_000_000;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn report(lnd: Option<LndTags>) -> HealthReport {
        HealthReport {
            status: HealthStatus::Down,
            checks: vec![
                (CheckName::LndSync, CheckState::Fail, NOW - 3_600),
                (CheckName::Payouts, CheckState::Fail, NOW - 600),
                (CheckName::Relays, CheckState::Ok, NOW - 86_400),
                (CheckName::Database, CheckState::Ok, NOW - 86_400),
            ],
            escrow_mode: "lightning",
            lnd,
            maintenance: true,
            instance_name: Some("Mostro".to_string()),
            interval: 60,
        }
    }

    fn lnd() -> Option<LndTags> {
        Some(LndTags {
            block_height: 971_349,
            synced_to_chain: false,
            network: "mainnet".to_string(),
        })
    }

    fn values<'a>(tags: &'a [Tag], name: &str) -> Vec<&'a [String]> {
        tags.iter()
            .map(|t| t.as_slice())
            .filter(|t| t.first().map(String::as_str) == Some(name))
            .collect()
    }

    fn value(tags: &[Tag], name: &str) -> Option<String> {
        values(tags, name).first().and_then(|t| t.get(1)).cloned()
    }

    #[test]
    fn health_event_expiration_is_three_intervals() {
        let keys = Keys::generate();
        let stamp = Stamp {
            seq: 7,
            created_at: NOW,
        };
        let event = build_health_event(&keys, &report(lnd()), stamp, "{}", None).unwrap();
        assert_eq!(event.created_at.as_secs() as i64, NOW);
        assert_eq!(
            value(event.tags.as_slice(), "expiration"),
            Some((NOW + 180).to_string())
        );
        assert_eq!(
            value(event.tags.as_slice(), "interval").as_deref(),
            Some("60")
        );
        assert_eq!(value(event.tags.as_slice(), "seq").as_deref(), Some("7"));
        assert_eq!(event.kind, Kind::Custom(38389));
        event.verify().unwrap();
    }

    #[test]
    fn health_event_is_addressed_by_the_publisher_and_tagged_health() {
        let keys = Keys::generate();
        let event = build_health_event(
            &keys,
            &report(lnd()),
            Stamp {
                seq: 1,
                created_at: NOW,
            },
            "{}",
            None,
        )
        .unwrap();
        let tags = event.tags.as_slice();
        assert_eq!(value(tags, "d"), Some(keys.public_key().to_hex()));
        assert_eq!(value(tags, "z").as_deref(), Some("health"));
        assert_eq!(
            values(tags, "y")[0],
            &["y".to_string(), "mostro".to_string(), "Mostro".to_string()]
        );
        assert_eq!(value(tags, "status").as_deref(), Some("down"));
        assert_eq!(value(tags, "escrow_mode").as_deref(), Some("lightning"));
        assert_eq!(value(tags, "maintenance_mode").as_deref(), Some("true"));
        assert_eq!(
            value(tags, "mostro_version").as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        let checks = values(tags, "check");
        assert_eq!(checks.len(), 4);
        assert_eq!(
            checks[0],
            &[
                "check".to_string(),
                "lnd_sync".to_string(),
                "fail".to_string(),
                (NOW - 3_600).to_string()
            ]
        );
    }

    #[test]
    fn health_event_content_empty_without_watchdog_pubkey() {
        let keys = Keys::generate();
        let stamp = Stamp {
            seq: 1,
            created_at: NOW,
        };
        let event = build_health_event(&keys, &report(lnd()), stamp, r#"{"v":1}"#, None).unwrap();
        assert_eq!(event.content, "");
        assert!(values(event.tags.as_slice(), "p").is_empty());
    }

    #[test]
    fn health_event_content_decrypts_for_the_watchdog() {
        let keys = Keys::generate();
        let watchdog = Keys::generate();
        let stamp = Stamp {
            seq: 1,
            created_at: NOW,
        };
        let payload = r#"{"v":1,"generated_at":1792000000}"#;
        let event = build_health_event(
            &keys,
            &report(lnd()),
            stamp,
            payload,
            Some(&watchdog.public_key()),
        )
        .unwrap();
        let p = values(event.tags.as_slice(), "p");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0][1], watchdog.public_key().to_hex());
        let plaintext =
            nip44::decrypt(watchdog.secret_key(), &keys.public_key(), &event.content).unwrap();
        assert_eq!(plaintext, payload);
    }

    #[test]
    fn lnd_tags_appear_and_disappear_together() {
        let keys = Keys::generate();
        let stamp = Stamp {
            seq: 1,
            created_at: NOW,
        };
        let with = health_tags(&keys.public_key(), &report(lnd()), stamp, None);
        assert_eq!(value(&with, "lnd_block_height").as_deref(), Some("971349"));
        assert_eq!(
            value(&with, "lnd_synced_to_chain").as_deref(),
            Some("false")
        );
        assert_eq!(value(&with, "lnd_network").as_deref(), Some("mainnet"));

        let without = health_tags(&keys.public_key(), &report(None), stamp, None);
        for name in ["lnd_block_height", "lnd_synced_to_chain", "lnd_network"] {
            assert!(values(&without, name).is_empty(), "{name} must be omitted");
        }
    }

    #[test]
    fn no_tag_is_repeated() {
        let keys = Keys::generate();
        let mut report = report(lnd());
        // Even a caller that passes a check twice gets it once.
        report
            .checks
            .push((CheckName::Relays, CheckState::Warn, NOW));
        let tags = health_tags(
            &keys.public_key(),
            &report,
            Stamp {
                seq: 1,
                created_at: NOW,
            },
            Some(&Keys::generate().public_key()),
        );
        let mut counts: HashMap<String, usize> = HashMap::new();
        for tag in &tags {
            let slice = tag.as_slice();
            let key = if slice[0] == "check" {
                format!("check:{}", slice[1])
            } else {
                slice[0].clone()
            };
            *counts.entry(key).or_default() += 1;
        }
        for (key, count) in counts {
            assert_eq!(count, 1, "{key} appears {count} times");
        }
    }

    #[test]
    fn check_since_never_exceeds_created_at() {
        let keys = Keys::generate();
        let mut report = report(lnd());
        report.checks[0].2 = NOW + 500;
        let tags = health_tags(
            &keys.public_key(),
            &report,
            Stamp {
                seq: 1,
                created_at: NOW,
            },
            None,
        );
        assert_eq!(values(&tags, "check")[0][3], NOW.to_string());
    }

    #[test]
    fn first_stamp_starts_at_one_and_now() {
        assert_eq!(
            next_stamp(None, NOW),
            (
                Stamp {
                    seq: 1,
                    created_at: NOW
                },
                false
            )
        );
    }

    #[test]
    fn created_at_keeps_rising_when_the_clock_steps_back() {
        let last = Stamp {
            seq: 41,
            created_at: NOW,
        };
        // Clock stepped back 2 minutes (NTP correction): still after the last.
        let (stamp, ahead) = next_stamp(Some(last), NOW - 120);
        assert_eq!(
            stamp,
            Stamp {
                seq: 42,
                created_at: NOW + 1
            }
        );
        assert!(!ahead, "121s ahead is within what readers tolerate");

        // Two publications in the same second still differ.
        let (stamp, _) = next_stamp(Some(last), NOW);
        assert_eq!(stamp.created_at, NOW + 1);

        // A normal clock just moves forward.
        let (stamp, _) = next_stamp(Some(last), NOW + 60);
        assert_eq!(stamp.created_at, NOW + 60);
    }

    #[test]
    fn a_large_clock_step_back_is_flagged() {
        let last = Stamp {
            seq: 41,
            created_at: NOW,
        };
        let (stamp, ahead) = next_stamp(Some(last), NOW - 3_600);
        assert_eq!(stamp.created_at, NOW + 1);
        assert!(ahead, "the event is more than 300s in the future");
    }

    #[tokio::test]
    async fn health_event_seq_increases_across_restarts() {
        let pool = pool().await;
        assert_eq!(load_persisted(&pool).await.unwrap().last_stamp, None);

        let (first, _) = next_stamp(None, NOW);
        save_stamp(&pool, first).await.unwrap();
        let (second, _) = next_stamp(Some(first), NOW + 60);
        save_stamp(&pool, second).await.unwrap();

        // Restart, with the clock set back by an hour.
        let restored = load_persisted(&pool).await.unwrap().last_stamp;
        assert_eq!(restored, Some(second));
        let (third, _) = next_stamp(restored, NOW - 3_600);
        assert!(third.seq > second.seq);
        assert!(third.created_at > second.created_at);
    }

    #[test]
    fn relays_connecting_at_boot_are_not_an_outage() {
        let none = RelayOutage::default();
        assert_eq!(none.observe(false, 0, NOW), None);
    }

    #[test]
    fn losing_the_last_relay_starts_an_outage_and_a_connection_ends_it() {
        let none = RelayOutage::default();
        let started = none.observe(true, 0, NOW).unwrap();
        assert_eq!(
            started,
            RelayOutage {
                started_at: Some(NOW),
                ended_at: None
            }
        );
        // Still down: nothing changes, the start stays put.
        assert_eq!(started.observe(true, 0, NOW + 15), None);
        let ended = started.observe(true, 2, NOW + 120).unwrap();
        assert_eq!(
            ended,
            RelayOutage {
                started_at: Some(NOW),
                ended_at: Some(NOW + 120)
            }
        );
        assert_eq!(ended.observe(true, 2, NOW + 135), None);
        // A new outage replaces the previous one.
        let again = ended.observe(true, 0, NOW + 600).unwrap();
        assert_eq!(
            again,
            RelayOutage {
                started_at: Some(NOW + 600),
                ended_at: None
            }
        );
    }

    #[tokio::test]
    async fn an_outage_survives_a_restart_and_ends_on_reconnection() {
        let pool = pool().await;
        let started = RelayOutage::default().observe(true, 0, NOW).unwrap();
        save_relay_outage(&pool, started).await.unwrap();

        // Restart while still cut off; relays come back after boot.
        let restored = load_persisted(&pool).await.unwrap().relay_outage;
        assert_eq!(restored, started);
        let ended = restored.observe(false, 1, NOW + 300).unwrap();
        save_relay_outage(&pool, ended).await.unwrap();
        assert_eq!(
            load_persisted(&pool).await.unwrap().relay_outage,
            RelayOutage {
                started_at: Some(NOW),
                ended_at: Some(NOW + 300)
            }
        );
    }
}
