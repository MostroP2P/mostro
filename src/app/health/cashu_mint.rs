//! The `cashu_mint` check of the health event (Cashu instances only;
//! protocol `health_event.md`, "`cashu_mint`").
//!
//! The daemon cannot escrow anything while its mint is down, even with
//! healthy relays and database. The mints probed are the `[cashu].mint_urls`
//! allowlist, or, on an open node (empty allowlist), the distinct mints of
//! the orders in progress. A mint counts as reachable when it answers
//! `GET /v1/info` (NUT-06) with a success status within 10 seconds.

use crate::app::health::check::CheckState;
use crate::cashu::mint_answers_info;
use crate::cashu::mint_policy::{normalize_mint_url, public_mint_addr};
use crate::config::types::CashuSettings;
use crate::db::TERMINAL_ORDER_STATUSES;
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

/// Distinct mints of the orders in progress (not pending, not terminal),
/// canonicalised so one mint is probed once.
pub async fn in_progress_mints(pool: &SqlitePool) -> Result<Vec<String>, MostroError> {
    // The status list is a compile-time constant, never user input.
    let query = format!(
        "SELECT DISTINCT cashu_mint_url FROM orders \
         WHERE cashu_mint_url IS NOT NULL AND cashu_mint_url != '' \
           AND status != 'pending' AND status NOT IN ({TERMINAL_ORDER_STATUSES})"
    );
    let urls: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(query))
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
        match in_progress_mints(pool).await {
            Ok(mints) => mints,
            Err(e) => {
                warn!("Health event: cannot list the mints of orders in progress: {e}");
                return CheckState::Unknown;
            }
        }
    } else {
        cashu.mint_urls.clone()
    };
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

    async fn insert(pool: &SqlitePool, status: &str, mint: &str) {
        sqlx::query(
            "INSERT INTO orders (id, kind, event_id, status, premium, payment_method, amount, \
             fiat_code, fiat_amount, created_at, expires_at, cashu_mint_url) \
             VALUES (?, 'sell', 'e', ?, 0, 'cash', 1000, 'USD', 10, 0, 0, ?)",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(status)
        .bind(mint)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn open_node_probes_the_mints_of_orders_in_progress() {
        let pool = pool().await;
        insert(&pool, "active", "https://mint.example.com").await;
        insert(&pool, "fiat-sent", "https://MINT.example.com/").await;
        insert(&pool, "waiting-payment", "https://other.example.org").await;
        insert(&pool, "pending", "https://pending.example.net").await;
        insert(&pool, "success", "https://done.example.net").await;

        let mints = in_progress_mints(&pool).await.unwrap();
        assert_eq!(mints.len(), 2, "one per mint, in progress only: {mints:?}");
        assert!(mints
            .iter()
            .all(|m| !m.contains("pending") && !m.contains("done")));
    }

    #[tokio::test]
    async fn open_node_with_no_order_in_progress_is_ok() {
        let pool = pool().await;
        insert(&pool, "pending", "https://pending.example.net").await;
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
        insert(&pool, "active", &format!("http://{addr}")).await;
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
