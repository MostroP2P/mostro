//! The `cashu_mint` check of the health event (Cashu instances only;
//! protocol `health_event.md`, "`cashu_mint`").
//!
//! The daemon cannot escrow anything while its mint is down, even with
//! healthy relays and database. The mints probed are the `[cashu].mint_urls`
//! allowlist, or, on an open node (empty allowlist), the distinct mints that
//! currently hold escrow: orders past the escrow step, with a token locked,
//! not yet terminal. Never a mint a pending order merely names: anyone can
//! publish an order naming an unreachable mint, and that must not make the
//! instance look down. A mint counts as reachable when it answers
//! `GET /v1/info` (NUT-06) with a success status within 10 seconds.

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

/// Probe one mint. A mint a maker chose (open node) gets the same public-host
/// check as when the daemon connects to it, and the request is pinned to the
/// address that passed it.
async fn probe(mint_url: &str, open: bool) -> bool {
    if !open {
        return mint_answers_info(mint_url, None).await;
    }
    match public_mint_addr(mint_url).await {
        Ok(addr) => mint_answers_info(mint_url, Some(addr)).await,
        Err(_) => false,
    }
}

/// The `cashu_mint` check for this node.
pub async fn cashu_mint_check(pool: &SqlitePool, cashu: &CashuSettings) -> CheckState {
    let open = cashu.mint_urls.is_empty();
    let mints = if open {
        match escrow_mints(pool).await {
            Ok(mints) => mints,
            Err(e) => {
                warn!("Health event: cannot list the mints holding escrow: {e}");
                return CheckState::Unknown;
            }
        }
    } else {
        cashu.mint_urls.clone()
    };
    probe_mints(&mints, open).await
}

/// Probe `mints` (the allowlist, or on an open node the mints holding
/// escrow) and derive the check. Network only: the health publisher reads
/// the open node's mint list within its database budget and calls this.
pub async fn probe_mints(mints: &[String], open: bool) -> CheckState {
    let probes = mints.iter().map(|mint| probe(mint, open));
    let reachable = futures::future::join_all(probes).await;
    for (mint, up) in mints.iter().zip(&reachable) {
        if !up {
            warn!("Health event: Cashu mint {mint} did not answer /v1/info");
        }
    }
    cashu_mint_state(&reachable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
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
