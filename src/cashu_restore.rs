//! Cashu escrow restore / monitor — Track A **TA-3**
//! (see `docs/cashu/02-track-a-lock.md` §6/§10).
//!
//! After a restart in Cashu mode the daemon re-reads the in-flight locked
//! escrows from the DB — the Cashu analogue of the Lightning path's
//! `find_held_invoices` resubscribe — and re-checks each token (NUT-07) at the
//! mint its order escrows on, to surface any escrow that was redeemed or
//! double-spent while the daemon was down.
//!
//! Best-effort and log-only: it never mutates order state (that belongs to the
//! release/cancel/dispute tracks). The boot path runs it in the background, so
//! a slow mint delays this monitor and never the daemon.

use crate::cashu::mint_policy::resolve_order_mint;
use crate::cashu::mints::CashuMints;
use crate::db::find_locked_cashu_orders;
use futures::stream::{self, StreamExt};
use mostro_core::error::CantDoReason;
use mostro_core::order::Order;
use sqlx::SqlitePool;

/// Most escrows checked at once, so a node with many in-flight trades does not
/// flood a mint, while one unresponsive mint cannot hold up the whole pass.
const MAX_CONCURRENT_CHECKS: usize = 8;

/// What the mint check found for one locked escrow.
#[derive(Debug, PartialEq, Eq)]
enum EscrowCheck {
    /// Every proof of the token is unspent at the order's mint.
    Live,
    /// At least one proof is spent or pending at the order's mint.
    SpentOrPending,
    /// The row is locked but has no token or no mint. The lock writes them
    /// together, so this is a data anomaly.
    MissingEscrowData,
    /// The node's mint policy refuses the order's mint now, for example the
    /// operator dropped it from `[cashu].mint_urls`. The mint is not contacted.
    MintNotChecked(CantDoReason),
    /// The mint could not answer. Says nothing about the escrow.
    CheckFailed(String),
}

/// Re-check every in-flight locked Cashu escrow, each at its own mint.
///
/// `allowed_mints` is `[cashu].mint_urls`; empty means an open node.
pub async fn restore_cashu_escrows(
    pool: &SqlitePool,
    mints: &CashuMints,
    allowed_mints: &[String],
) {
    let locked = match find_locked_cashu_orders(pool).await {
        Ok(orders) => orders,
        Err(e) => {
            tracing::warn!("cashu restore: failed to load locked escrows: {e}");
            return;
        }
    };

    if locked.is_empty() {
        tracing::info!("cashu restore: no in-flight locked escrows");
        return;
    }

    tracing::info!(
        "cashu restore: re-checking {} in-flight locked escrow(s)",
        locked.len()
    );

    stream::iter(&locked)
        .for_each_concurrent(MAX_CONCURRENT_CHECKS, |order| async move {
            log_check(order, &check_escrow(order, mints, allowed_mints).await);
        })
        .await;
}

/// Check one escrow at the mint its order escrows on. The mint is put through
/// the policy a new order gets: this reaches out to maker-chosen URLs at boot,
/// so a mint the node would refuse today is reported instead of contacted.
async fn check_escrow(order: &Order, mints: &CashuMints, allowed_mints: &[String]) -> EscrowCheck {
    let (Some(token), Some(mint)) = (
        order.cashu_escrow_token.as_deref(),
        order.cashu_mint_url.as_deref(),
    ) else {
        return EscrowCheck::MissingEscrowData;
    };
    let mint = match resolve_order_mint(Some(mint), allowed_mints).await {
        Ok(mint) => mint,
        Err(reason) => return unresolved_mint(reason),
    };
    let client = match mints.client_for(&mint).await {
        Ok(client) => client,
        Err(e) => return EscrowCheck::CheckFailed(e.to_string()),
    };
    match client.check_token_unspent(token).await {
        Ok(true) => EscrowCheck::Live,
        Ok(false) => EscrowCheck::SpentOrPending,
        Err(e) => EscrowCheck::CheckFailed(e.to_string()),
    }
}

/// A resolver timeout is transient and says nothing about the mint, so it must
/// not be reported as the node refusing it.
fn unresolved_mint(reason: CantDoReason) -> EscrowCheck {
    match reason {
        CantDoReason::CashuMintUnavailable => {
            EscrowCheck::CheckFailed("resolving the mint host timed out".to_string())
        }
        reason => EscrowCheck::MintNotChecked(reason),
    }
}

fn log_check(order: &Order, check: &EscrowCheck) {
    let (id, status) = (order.id, &order.status);
    match check {
        EscrowCheck::Live => {
            tracing::info!("cashu restore: order {id} escrow is live (status {status})")
        }
        EscrowCheck::SpentOrPending => tracing::warn!(
            "cashu restore: order {id} escrow token is spent/pending at the mint \
             (status {status}) — needs attention"
        ),
        EscrowCheck::MissingEscrowData => tracing::warn!(
            "cashu restore: order {id} is locked but lacks its escrow token or mint \
             (status {status}) — needs attention"
        ),
        EscrowCheck::MintNotChecked(reason) => tracing::warn!(
            "cashu restore: order {id} escrow not checked, its mint is refused ({reason:?}) \
             (status {status}) — needs attention"
        ),
        EscrowCheck::CheckFailed(e) => {
            tracing::warn!("cashu restore: order {id} mint state check failed: {e}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cashu::CashuClient;
    use cdk::mint_url::MintUrl;
    use cdk::nuts::nut00::Proof;
    use cdk::nuts::nut02::Id;
    use cdk::nuts::{CurrencyUnit, PublicKey, Token};
    use cdk::secret::Secret;
    use cdk::Amount;
    use mostro_core::db::Crud;
    use mostro_core::order::Status;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Barrier;

    const TEST_KEYSET_ID: &str = "009a1f293253e41e";
    const TEST_C: &str = "02698c4e2b5f9534cd0687d87513c759790cf829aa5739184a3e3735471fbda904";

    /// A mint that answers `/v1/checkstate` with one state for every queried Y.
    struct MockMint {
        url: String,
        hits: Arc<AtomicUsize>,
    }

    /// The JSON body of one HTTP request.
    async fn read_body(socket: &mut TcpStream) -> serde_json::Value {
        let mut raw = Vec::new();
        let mut chunk = [0u8; 2048];
        loop {
            if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&raw[..end]).to_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map(|v| v.trim().parse().unwrap())
                    .unwrap_or(0);
                if raw.len() >= end + 4 + length {
                    return serde_json::from_slice(&raw[end + 4..end + 4 + length]).unwrap();
                }
            }
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0, "connection closed mid-request");
            raw.extend_from_slice(&chunk[..read]);
        }
    }

    /// With a barrier, no answer leaves until enough requests are in flight.
    async fn mock_mint(state: &'static str, barrier: Option<Arc<Barrier>>) -> MockMint {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (counter, barrier) = (counter.clone(), barrier.clone());
                tokio::spawn(async move {
                    let request = read_body(&mut socket).await;
                    counter.fetch_add(1, Ordering::SeqCst);
                    if let Some(barrier) = barrier {
                        barrier.wait().await;
                    }
                    let states: Vec<_> = request["Ys"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|y| serde_json::json!({"Y": y, "state": state, "witness": null}))
                        .collect();
                    let body = serde_json::json!({ "states": states }).to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         connection: close\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                    let _ = socket.shutdown().await;
                });
            }
        });
        MockMint { url, hits }
    }

    fn token(mint: &str, secret: &str) -> String {
        let proof = Proof {
            keyset_id: Id::from_str(TEST_KEYSET_ID).unwrap(),
            amount: Amount::from(100u64),
            secret: Secret::new(secret.to_string()),
            c: PublicKey::from_str(TEST_C).unwrap(),
            witness: None,
            dleq: None,
            p2pk_e: None,
        };
        Token::new(
            MintUrl::from_str(mint).unwrap(),
            vec![proof],
            None,
            CurrencyUnit::Sat,
        )
        .to_string()
    }

    fn locked_order(mint: Option<&str>, escrow_token: Option<String>) -> Order {
        Order {
            id: uuid::Uuid::new_v4(),
            status: Status::Active.to_string(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            fiat_code: "USD".to_string(),
            amount: 100,
            fiat_amount: 40,
            cashu_mint_url: mint.map(str::to_string),
            cashu_escrow_token: escrow_token,
            cashu_escrow_locked_at: Some(1_700_000_100),
            ..Default::default()
        }
    }

    /// A node allowing exactly `urls`, with a client cached for each.
    async fn node_with(urls: &[&str]) -> (CashuMints, Vec<String>) {
        let mints = CashuMints::new(false);
        for url in urls {
            mints.insert(CashuClient::offline(url)).await;
        }
        (mints, urls.iter().map(|url| url.to_string()).collect())
    }

    /// Migrated pool on one connection: each `sqlite::memory:` connection is
    /// its own private database.
    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn checks_each_escrow_at_the_mint_of_its_order() {
        let live = mock_mint("UNSPENT", None).await;
        let spent = mock_mint("SPENT", None).await;
        let (mints, allowed) = node_with(&[&live.url, &spent.url]).await;
        let on_live = locked_order(Some(&live.url), Some(token(&live.url, "a")));
        let on_spent = locked_order(Some(&spent.url), Some(token(&spent.url, "b")));

        let first = check_escrow(&on_live, &mints, &allowed).await;
        let second = check_escrow(&on_spent, &mints, &allowed).await;

        assert_eq!(first, EscrowCheck::Live);
        assert_eq!(second, EscrowCheck::SpentOrPending);
        assert_eq!(live.hits.load(Ordering::SeqCst), 1);
        assert_eq!(spent.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_pending_proof_is_not_live() {
        let mint = mock_mint("PENDING", None).await;
        let (mints, allowed) = node_with(&[&mint.url]).await;
        let order = locked_order(Some(&mint.url), Some(token(&mint.url, "a")));

        let check = check_escrow(&order, &mints, &allowed).await;

        assert_eq!(check, EscrowCheck::SpentOrPending);
    }

    #[test]
    fn a_resolver_timeout_is_a_failed_check_not_a_refused_mint() {
        let timed_out = unresolved_mint(CantDoReason::CashuMintUnavailable);
        let refused = unresolved_mint(CantDoReason::InvalidMintUrl);

        assert!(matches!(timed_out, EscrowCheck::CheckFailed(_)));
        assert_eq!(
            refused,
            EscrowCheck::MintNotChecked(CantDoReason::InvalidMintUrl)
        );
    }

    /// Codex on #831: the operator changed the mints while an escrow was
    /// locked on the old one. It must not read as live, and the daemon must
    /// not keep calling a mint the operator removed.
    #[tokio::test]
    async fn never_contacts_a_mint_dropped_from_the_allowlist() {
        let dropped = mock_mint("UNSPENT", None).await;
        let (mints, _) = node_with(&[&dropped.url]).await;
        let allowed = vec!["https://other-mint.example.com".to_string()];
        let order = locked_order(Some(&dropped.url), Some(token(&dropped.url, "a")));

        let check = check_escrow(&order, &mints, &allowed).await;

        assert_eq!(
            check,
            EscrowCheck::MintNotChecked(CantDoReason::InvalidMintUrl)
        );
        assert_eq!(dropped.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn flags_a_locked_row_without_token_or_mint() {
        let (mints, allowed) = node_with(&[]).await;
        let no_token = locked_order(Some("https://mint.example.com"), None);
        let no_mint = locked_order(None, Some("cashuBtoken".to_string()));

        let first = check_escrow(&no_token, &mints, &allowed).await;
        let second = check_escrow(&no_mint, &mints, &allowed).await;

        assert_eq!(first, EscrowCheck::MissingEscrowData);
        assert_eq!(second, EscrowCheck::MissingEscrowData);
    }

    #[tokio::test]
    async fn an_unreachable_mint_is_a_failed_check_not_a_verdict() {
        let url = "http://127.0.0.1:1";
        let (mints, allowed) = node_with(&[url]).await;
        let order = locked_order(Some(url), Some(token(url, "a")));

        let check = check_escrow(&order, &mints, &allowed).await;

        assert!(matches!(check, EscrowCheck::CheckFailed(_)), "{check:?}");
    }

    #[tokio::test]
    async fn restore_over_empty_pool_never_contacts_a_mint() {
        let mint = mock_mint("UNSPENT", None).await;
        let (mints, allowed) = node_with(&[&mint.url]).await;

        restore_cashu_escrows(&pool().await, &mints, &allowed).await;

        assert_eq!(mint.hits.load(Ordering::SeqCst), 0);
    }

    /// Review on #831: serial checks cost N × the mint timeout. This mint
    /// answers only once both requests are in flight, so a serial pass stalls
    /// on the first one until the timeout below.
    #[tokio::test]
    async fn restore_checks_escrows_concurrently() {
        let mint = mock_mint("UNSPENT", Some(Arc::new(Barrier::new(2)))).await;
        let (mints, allowed) = node_with(&[&mint.url]).await;
        let pool = pool().await;
        for secret in ["a", "b"] {
            locked_order(Some(&mint.url), Some(token(&mint.url, secret)))
                .create(&pool)
                .await
                .unwrap();
        }

        let restore = restore_cashu_escrows(&pool, &mints, &allowed);
        tokio::time::timeout(Duration::from_secs(5), restore)
            .await
            .expect("escrows must be checked concurrently");

        assert_eq!(mint.hits.load(Ordering::SeqCst), 2);
    }

    /// Review on #831: the boot query must not walk every order ever made.
    #[tokio::test]
    async fn locked_orders_query_does_not_scan_every_order() {
        let pool = pool().await;
        let query = format!(
            "EXPLAIN QUERY PLAN {}",
            crate::db::locked_cashu_orders_query().0
        );

        let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .fetch_all(&pool)
            .await
            .unwrap();

        assert!(
            !plan.iter().any(|row| row.3.starts_with("SCAN orders")),
            "{plan:?}"
        );
    }
}
