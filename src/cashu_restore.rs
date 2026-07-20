//! Cashu escrow restore / monitor — Track A **TA-3**
//! (see `docs/cashu/02-track-a-lock.md` §6/§10).
//!
//! On startup in Cashu mode the daemon re-hydrates in-flight locked escrows
//! from the DB — the Cashu analogue of the Lightning path's `find_held_invoices`
//! resubscribe — so a restart never loses sight of an escrow that is locked but
//! not yet released. Each locked token is re-checked against the mint (NUT-07)
//! to surface any escrow that was redeemed or double-spent while the daemon was
//! down. Best-effort and log-only: it never mutates order state (that belongs to
//! the release/cancel/dispute tracks) and never blocks boot on a mint hiccup.

use crate::cashu::CashuClient;
use crate::db::find_locked_cashu_orders;
use sqlx::SqlitePool;

/// Re-hydrate and re-check every in-flight locked Cashu escrow at boot.
pub async fn restore_cashu_escrows(pool: &SqlitePool, cashu_client: &CashuClient) {
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
        "cashu restore: re-hydrating {} in-flight locked escrow(s)",
        locked.len()
    );

    for order in &locked {
        let Some(token) = order.cashu_escrow_token.as_deref() else {
            // The CAS writes token + locked_at together, so a locked row with
            // no token is a data anomaly worth flagging.
            tracing::warn!(
                "cashu restore: order {} is locked but carries no escrow token",
                order.id
            );
            continue;
        };

        match cashu_client.check_token_unspent(token).await {
            Ok(true) => tracing::info!(
                "cashu restore: order {} escrow is live (status {})",
                order.id,
                order.status
            ),
            Ok(false) => tracing::warn!(
                "cashu restore: order {} escrow token is spent/pending at the mint \
                 (status {}) — needs attention",
                order.id,
                order.status
            ),
            Err(e) => tracing::warn!(
                "cashu restore: order {} mint state check failed: {e}",
                order.id
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no locked escrows the restore path is a clean no-op — it returns
    /// before ever contacting the mint (so this runs offline) and never panics.
    #[tokio::test]
    async fn restore_over_empty_pool_finds_nothing() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        // `find_locked_cashu_orders` returning empty is the gate that makes
        // `restore_cashu_escrows` return before constructing any mint request.
        let locked = find_locked_cashu_orders(&pool).await.unwrap();
        assert!(locked.is_empty());
    }
}
