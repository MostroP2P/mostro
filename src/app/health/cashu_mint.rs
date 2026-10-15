//! The `cashu_mint` check of the health event (Cashu instances only;
//! protocol `health_event.md`, "`cashu_mint`").
//!
//! The daemon cannot escrow anything while its mint is down, even with
//! healthy relays and database. The mints probed are the union of the
//! `[cashu].mint_urls` allowlist and the distinct mints that currently hold
//! escrow (orders past the escrow step, with a token locked, not yet
//! terminal), even with a non-empty allowlist: a mint removed from the
//! allowlist while it still holds escrow keeps being probed. Only allowlisted
//! mints are reached directly; any other one passes the public-host check
//! and is reached at the address that passed it. Never a mint a
//! pending order merely names: anyone can publish an order naming an
//! unreachable mint, and that must not make the instance look down. Nothing
//! to probe (empty allowlist, no escrow held) is `ok`. A mint counts as
//! reachable when it answers `GET /v1/info` (NUT-06) with a success status
//! within 10 seconds.

use crate::app::health::check::CheckState;
use crate::cashu::mint_answers_info;
use crate::cashu::mint_policy::{normalize_mint_url, public_mint_addr};
use crate::config::types::CashuSettings;
use mostro_core::error::MostroError::{self, MostroInternalErr};
use mostro_core::error::ServiceError;
use sqlx::SqlitePool;
use std::collections::BTreeSet;
use tracing::warn;

/// The check from the probe results, one `bool` per probed mint.
pub fn cashu_mint_state(reachable: &[bool]) -> CheckState {
    let up = reachable.iter().filter(|up| **up).count();
    if up == reachable.len() {
        // Every mint answered, or there was none to probe.
        CheckState::Ok
    } else if up == 0 {
        CheckState::Fail
    } else {
        CheckState::Warn
    }
}

/// Order statuses an order with a locked escrow token can still be in: every
/// status that is neither terminal nor before the escrow step. Listed
/// positively (not as `NOT IN` the terminal ones) so the query below can
/// seek the partial index `idx_orders_cashu_escrow_status` per status
/// instead of walking every historical token. `escrow_holding_statuses_cover_every_status`
/// keeps it in step with `Status`.
pub(crate) const ESCROW_HOLDING_STATUSES: &str = "'waiting-buyer-invoice','waiting-payment',\
'active','fiat-sent','dispute','settled-hold-invoice','in-progress'";

/// Distinct mints that currently hold escrow: a locked escrow token on an
/// order that is not terminal. Canonicalised so one mint is probed once.
pub async fn escrow_mints(pool: &SqlitePool) -> Result<Vec<String>, MostroError> {
    let urls: Vec<String> = sqlx::query_scalar(escrow_mints_query())
        .fetch_all(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    Ok(urls
        .iter()
        .filter_map(|url| normalize_mint_url(url).ok())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

/// The query behind [`escrow_mints`]. The status list is a compile-time
/// constant, never user input.
pub(crate) fn escrow_mints_query() -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(format!(
        "SELECT DISTINCT cashu_mint_url FROM orders \
         WHERE cashu_escrow_token IS NOT NULL AND cashu_escrow_token != '' \
           AND cashu_mint_url IS NOT NULL AND cashu_mint_url != '' \
           AND status IN ({ESCROW_HOLDING_STATUSES})"
    ))
}

/// A mint to probe, and how to reach it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MintTarget {
    /// Canonical URL ([`normalize_mint_url`]).
    pub url: String,
    /// In the current `[cashu].mint_urls` allowlist: the operator chose it,
    /// so it is reached directly. Any other mint (on an open node, or one
    /// removed from the allowlist while it still holds escrow) was chosen by
    /// a maker: its host may since resolve to a private address, so it gets
    /// the public-host check on every probe.
    pub listed: bool,
}

/// Probe one mint. A mint not in the allowlist gets the same public-host
/// check as when an open node connects to it, and the request is pinned to
/// the address that passed it: one whose host now resolves to a private
/// address (loopback, link-local metadata, LAN) is never contacted and
/// counts as unreachable.
async fn probe(mint: &MintTarget) -> bool {
    if mint.listed {
        return mint_answers_info(&mint.url, None).await;
    }
    match public_mint_addr(&mint.url).await {
        Ok(addr) => mint_answers_info(&mint.url, Some(addr)).await,
        Err(_) => false,
    }
}

/// The `cashu_mint` check for this node.
pub async fn cashu_mint_check(pool: &SqlitePool, cashu: &CashuSettings) -> CheckState {
    let escrow = match escrow_mints(pool).await {
        Ok(mints) => mints,
        Err(e) => {
            warn!("Health event: cannot list the mints holding escrow: {e}");
            return CheckState::Unknown;
        }
    };
    probe_mints(&mints_to_probe(&cashu.mint_urls, escrow)).await
}

/// The probe set: the allowlist plus the mints holding escrow (`escrow`,
/// canonical), without duplicates, each marked with whether it is in the
/// allowlist.
pub fn mints_to_probe(allowlist: &[String], escrow: Vec<String>) -> Vec<MintTarget> {
    let listed: BTreeSet<String> = allowlist
        .iter()
        .filter_map(|url| normalize_mint_url(url).ok())
        .collect();
    let unlisted: BTreeSet<String> = escrow
        .into_iter()
        .filter(|url| !listed.contains(url))
        .collect();
    listed
        .into_iter()
        .map(|url| MintTarget { url, listed: true })
        .chain(
            unlisted
                .into_iter()
                .map(|url| MintTarget { url, listed: false }),
        )
        .collect()
}

/// Probe `mints` and derive the check. Network only: the health publisher
/// reads the mints holding escrow within its database budget, builds the set
/// with [`mints_to_probe`] and calls this.
pub async fn probe_mints(mints: &[MintTarget]) -> CheckState {
    let reachable = futures::future::join_all(mints.iter().map(probe)).await;
    for (mint, up) in mints.iter().zip(&reachable) {
        if !up {
            warn!(
                "Health event: Cashu mint {} did not answer /v1/info",
                mint.url
            );
        }
    }
    cashu_mint_state(&reachable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A mock mint that answers `GET /v1/info` once with a minimal NUT-06 body.
    async fn mock_mint() -> SocketAddr {
        let body = r#"{"name":"mock mint","nuts":{}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        addr
    }

    /// A loopback port with nothing listening.
    async fn dead_port() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        addr
    }

    fn settings(mint_urls: Vec<String>) -> CashuSettings {
        CashuSettings {
            enabled: true,
            mint_urls,
            ..Default::default()
        }
    }

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    /// Keeps [`ESCROW_HOLDING_STATUSES`] in step with `Status`: a new status
    /// must be classified here (the match is exhaustive) before it compiles.
    #[test]
    fn escrow_holding_statuses_cover_every_status() {
        use mostro_core::order::Status;
        let holds_escrow = |status: Status| match status {
            Status::WaitingBuyerInvoice
            | Status::WaitingPayment
            | Status::Active
            | Status::FiatSent
            | Status::Dispute
            | Status::SettledHoldInvoice
            | Status::InProgress => true,
            // Before the escrow step, or terminal.
            Status::Pending
            | Status::WaitingMakerBond
            | Status::WaitingTakerBond
            | Status::Canceled
            | Status::CanceledByAdmin
            | Status::SettledByAdmin
            | Status::CompletedByAdmin
            | Status::Expired
            | Status::Success
            | Status::CooperativelyCanceled => false,
        };
        for status in [
            Status::WaitingBuyerInvoice,
            Status::WaitingPayment,
            Status::Active,
            Status::FiatSent,
            Status::Dispute,
            Status::SettledHoldInvoice,
            Status::InProgress,
            Status::Pending,
            Status::WaitingMakerBond,
            Status::WaitingTakerBond,
            Status::Canceled,
            Status::CanceledByAdmin,
            Status::SettledByAdmin,
            Status::CompletedByAdmin,
            Status::Expired,
            Status::Success,
            Status::CooperativelyCanceled,
        ] {
            let listed = ESCROW_HOLDING_STATUSES.contains(&format!("'{status}'"));
            assert_eq!(listed, holds_escrow(status), "{status}");
            let terminal = crate::db::TERMINAL_ORDER_STATUSES.contains(&format!("'{status}'"));
            assert!(!(listed && terminal), "{status} cannot be both");
        }
    }

    /// Codex review on #1066: the escrow mint query seeks the partial index
    /// per status instead of walking every historical token.
    #[tokio::test]
    async fn escrow_mints_query_uses_the_partial_index() {
        let pool = pool().await;
        let plan: Vec<String> = sqlx::query_as::<_, (i64, i64, i64, String)>(sqlx::AssertSqlSafe(
            format!("EXPLAIN QUERY PLAN {}", escrow_mints_query().0),
        ))
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, _, _, detail)| detail)
        .collect();
        assert!(
            plan.iter()
                .any(|d| d.contains("idx_orders_cashu_escrow_status")),
            "{plan:?}"
        );
        assert!(
            !plan.iter().any(|d| d.starts_with("SCAN orders")),
            "{plan:?}"
        );
    }

    #[test]
    fn state_follows_the_probes() {
        assert_eq!(cashu_mint_state(&[]), CheckState::Ok);
        assert_eq!(cashu_mint_state(&[true, true]), CheckState::Ok);
        assert_eq!(cashu_mint_state(&[true, false]), CheckState::Warn);
        assert_eq!(cashu_mint_state(&[false, false]), CheckState::Fail);
    }

    #[tokio::test]
    async fn a_mint_answering_v1_info_is_reachable() {
        let addr = mock_mint().await;
        assert!(mint_answers_info(&format!("http://{addr}"), None).await);
        assert!(!mint_answers_info(&format!("http://{}", dead_port().await), None).await);
    }

    #[tokio::test]
    async fn allowlist_with_one_mint_down_warns() {
        let pool = pool().await;
        let up = mock_mint().await;
        let down = dead_port().await;
        let cashu = settings(vec![format!("http://{up}"), format!("http://{down}")]);
        assert_eq!(cashu_mint_check(&pool, &cashu).await, CheckState::Warn);
    }

    #[tokio::test]
    async fn allowlist_with_every_mint_down_fails() {
        let pool = pool().await;
        let cashu = settings(vec![format!("http://{}", dead_port().await)]);
        assert_eq!(cashu_mint_check(&pool, &cashu).await, CheckState::Fail);
    }

    /// A loopback mint that counts the connections it receives.
    async fn counting_mint() -> (SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                counter.fetch_add(1, AtomicOrdering::SeqCst);
                let body = r#"{"name":"mock mint","nuts":{}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let mut buf = [0u8; 2048];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (addr, hits)
    }

    /// Codex review on #1066 (SSRF): with an allowlist, a mint that holds
    /// escrow but is not listed (a maker's choice from when the node was
    /// open, or a de-listed one) still gets the public-host check. One that
    /// resolves to loopback answers, but is never contacted and counts as
    /// unreachable.
    #[tokio::test]
    async fn an_unlisted_escrow_mint_on_loopback_is_never_contacted() {
        let pool = pool().await;
        let (private, hits) = counting_mint().await;
        insert(&pool, "active", &format!("http://{private}"), true).await;
        let listed = format!("http://{}", mock_mint().await);

        let state = cashu_mint_check(&pool, &settings(vec![listed])).await;

        assert_eq!(state, CheckState::Warn, "listed up, unlisted unreachable");
        assert_eq!(hits.load(AtomicOrdering::SeqCst), 0, "never contacted");
    }

    /// The same for the cloud metadata address: rejected by the host check
    /// at once, without a connection attempt or a 10 s timeout.
    #[tokio::test]
    async fn an_unlisted_escrow_mint_on_the_metadata_address_is_never_contacted() {
        let pool = pool().await;
        insert(&pool, "active", "http://169.254.169.254", true).await;
        let listed = format!("http://{}", mock_mint().await);

        let started = std::time::Instant::now();
        let state = cashu_mint_check(&pool, &settings(vec![listed])).await;

        assert_eq!(state, CheckState::Warn);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    /// A listed mint is still reached directly, even on loopback: the
    /// operator chose it.
    #[tokio::test]
    async fn a_listed_mint_holding_escrow_is_reached_directly() {
        let pool = pool().await;
        let (listed, hits) = counting_mint().await;
        let listed = format!("http://{listed}");
        insert(&pool, "active", &listed, true).await;

        let state = cashu_mint_check(&pool, &settings(vec![listed])).await;

        assert_eq!(state, CheckState::Ok);
        assert_eq!(hits.load(AtomicOrdering::SeqCst), 1, "probed once");
    }

    /// Spec update on protocol #84: a mint removed from the allowlist while
    /// it still holds escrow keeps being probed, alongside the allowlist.
    /// (Here it is unreachable twice over: on loopback, it also fails the
    /// public-host check every unlisted mint gets.)
    #[tokio::test]
    async fn a_delisted_mint_still_holding_escrow_is_probed() {
        let pool = pool().await;
        let delisted = format!("http://{}", dead_port().await);
        insert(&pool, "active", &delisted, true).await;
        let listed = format!("http://{}", mock_mint().await);
        let cashu = settings(vec![listed]);

        assert_eq!(cashu_mint_check(&pool, &cashu).await, CheckState::Warn);
    }

    #[test]
    fn the_probe_set_is_the_allowlist_plus_the_escrow_mints() {
        let set = mints_to_probe(
            &[
                "https://Mint.example.com/".to_string(),
                "https://b.example.org".to_string(),
            ],
            vec![
                "https://mint.example.com".to_string(),
                "https://delisted.example.net".to_string(),
            ],
        );
        let target = |url: &str, listed: bool| MintTarget {
            url: url.to_string(),
            listed,
        };
        assert_eq!(
            set,
            vec![
                target("https://b.example.org", true),
                target("https://mint.example.com", true),
                target("https://delisted.example.net", false),
            ],
            "an escrow mint that is also listed is reached as listed"
        );
        assert_eq!(
            mints_to_probe(&[], vec!["https://maker.example.net".to_string()]),
            vec![target("https://maker.example.net", false)],
            "open node: every mint is unlisted"
        );
        assert!(mints_to_probe(&[], vec![]).is_empty());
    }

    #[tokio::test]
    async fn allowlist_with_every_mint_up_is_ok() {
        let pool = pool().await;
        let cashu = settings(vec![format!("http://{}", mock_mint().await)]);
        assert_eq!(cashu_mint_check(&pool, &cashu).await, CheckState::Ok);
    }

    /// An order on `mint`; `locked` gives it an escrow token, as the escrow
    /// step does.
    async fn insert(pool: &SqlitePool, status: &str, mint: &str, locked: bool) {
        let id = uuid::Uuid::new_v4();
        let token = locked.then(|| format!("cashuB-{id}"));
        sqlx::query(
            "INSERT INTO orders (id, kind, event_id, status, premium, payment_method, amount, \
             fiat_code, fiat_amount, created_at, expires_at, cashu_mint_url, \
             cashu_escrow_token, cashu_escrow_locked_at) \
             VALUES (?, 'sell', 'e', ?, 0, 'cash', 1000, 'USD', 10, 0, 0, ?, ?, ?)",
        )
        .bind(id)
        .bind(status)
        .bind(mint)
        .bind(&token)
        .bind(token.as_ref().map(|_| 1_792_000_000_i64))
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn open_node_probes_only_the_mints_holding_escrow() {
        let pool = pool().await;
        insert(&pool, "active", "https://mint.example.com", true).await;
        insert(&pool, "fiat-sent", "https://MINT.example.com/", true).await;
        insert(&pool, "dispute", "https://other.example.org", true).await;
        // Named but not escrowed yet: the seller has not locked the token.
        insert(
            &pool,
            "waiting-payment",
            "https://waiting.example.net",
            false,
        )
        .await;
        insert(&pool, "pending", "https://pending.example.net", false).await;
        // Terminal, even with a token.
        insert(&pool, "success", "https://done.example.net", true).await;

        let mints = escrow_mints(&pool).await.unwrap();
        assert_eq!(mints.len(), 2, "one per mint holding escrow: {mints:?}");
        assert!(mints
            .iter()
            .all(|m| !m.contains("waiting") && !m.contains("pending") && !m.contains("done")));
    }

    #[tokio::test]
    async fn open_node_with_no_escrow_is_ok() {
        let pool = pool().await;
        insert(&pool, "pending", "https://pending.example.net", false).await;
        assert_eq!(
            cashu_mint_check(&pool, &settings(vec![])).await,
            CheckState::Ok
        );
    }

    /// Spec update on protocol #84: anyone can publish a pending order naming
    /// an unreachable mint; that must not make the instance look down.
    #[tokio::test]
    async fn a_pending_order_naming_a_dead_mint_keeps_the_check_ok() {
        let pool = pool().await;
        let dead = dead_port().await;
        insert(&pool, "pending", &format!("http://{dead}"), false).await;
        insert(&pool, "waiting-payment", &format!("http://{dead}"), false).await;
        assert_eq!(
            cashu_mint_check(&pool, &settings(vec![])).await,
            CheckState::Ok
        );
    }

    #[tokio::test]
    async fn open_node_never_probes_a_private_mint() {
        // A loopback mint chosen by a maker fails the public-host check, so
        // it counts as unreachable even though it answers.
        let pool = pool().await;
        let addr = mock_mint().await;
        insert(&pool, "active", &format!("http://{addr}"), true).await;
        assert_eq!(
            cashu_mint_check(&pool, &settings(vec![])).await,
            CheckState::Fail
        );
    }

    #[tokio::test]
    async fn open_node_is_unknown_when_the_orders_cannot_be_read() {
        let pool = pool().await;
        sqlx::query("DROP TABLE orders")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            cashu_mint_check(&pool, &settings(vec![])).await,
            CheckState::Unknown
        );
    }
}
