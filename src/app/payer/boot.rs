//! Boot-time duties of the payer-history feature
//! (`docs/PAYER_HISTORY_ANTI_TRIANGULATION.md` §10.7, §13).

use super::db::{self, RecomputeOutcome};
use crate::config::payer_history::PayerHistorySettings;
use mostro_core::error::MostroError;
use nostr_sdk::prelude::Keys;
use sqlx::{Pool, Sqlite};

/// Re-evaluate every stored `experienced` snapshot when the configured
/// thresholds differ from the ones they were evaluated under (§10.7), or
/// when the node secret differs from the one the history was keyed with
/// (compared through [`super::node_key_id`], since the public key cannot
/// tell `s` from `n - s`): every history row is keyed by that secret, so
/// under a new one no declaration can reach it, and the recompute discards
/// the history.
///
/// Returns `None` when there was nothing to do: the feature is off (the
/// recompute never runs then, whatever the config says, D-10) or the stored
/// policy already matches. The first boot with the feature on finds no
/// policy row and recomputes once over an empty table, which seeds it.
pub async fn sync_experience_policy(
    pool: &Pool<Sqlite>,
    node_keys: &Keys,
    cfg: Option<&PayerHistorySettings>,
    now: i64,
) -> Result<Option<RecomputeOutcome>, MostroError> {
    if !PayerHistorySettings::enabled(cfg) {
        return Ok(None);
    }
    let (min_trades, min_days) = PayerHistorySettings::experience_thresholds(cfg);
    let node_key_id = super::node_key_id(node_keys);
    if let Some(policy) = db::load_experience_policy(pool).await? {
        if (policy.min_trades, policy.min_days) == (min_trades, min_days)
            && policy.node_key_id.as_deref() == Some(node_key_id.as_str())
        {
            return Ok(None);
        }
    }
    let outcome = db::recompute_experienced(pool, node_keys, min_trades, min_days, now).await?;
    tracing::info!(
        "payer_history: experience policy {min_trades}/{min_days} under the current node key; \
         recomputed snapshots, {} row(s) changed",
        outcome.changed
    );
    if outcome.discarded > 0 {
        // Expected once after a deliberate change of the node secret; a
        // surprise otherwise, and the history does not come back.
        tracing::warn!(
            "payer_history: the node secret changed; discarded {} payer history row(s) \
             keyed by the previous one, buyers' payment-account history starts over",
            outcome.discarded
        );
    }
    if outcome.unresolved > 0 {
        // Never the rows themselves: on a healthy node this is zero, and
        // anything else means an imported database.
        tracing::warn!(
            "payer_history: {} snapshot(s) could not be re-evaluated (unknown counterparty); \
             they no longer count as distinct or experienced counterparties",
            outcome.unresolved
        );
    }
    Ok(Some(outcome))
}

/// The boot warning for a node that enables payer history in Cashu mode
/// (§13): the Cashu success path does not call the history hook yet, so the
/// feature would collect declarations that never become history.
pub fn cashu_conflict_warning(
    cashu_enabled: bool,
    payer_history_enabled: bool,
) -> Option<&'static str> {
    (cashu_enabled && payer_history_enabled).then_some(
        "[payer_history] is enabled but this node runs in Cashu escrow mode, where payer \
         history is not wired yet: declare-payer and payment-history answer invalid_action \
         and no history is recorded",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::payer::db::{bump_history, load_experience_policy};
    use crate::app::payer::test_support::{create_test_pool, hash, payer_settings};

    fn with_thresholds(n: u32, d: u32) -> PayerHistorySettings {
        PayerHistorySettings {
            experienced_min_trades: n,
            experienced_min_days: d,
            ..payer_settings(true, false)
        }
    }

    #[tokio::test]
    async fn disabled_feature_never_recomputes() {
        let pool = create_test_pool().await;
        let keys = Keys::generate();
        let off = PayerHistorySettings {
            experienced_min_trades: 1,
            ..payer_settings(false, false)
        };
        for cfg in [None, Some(&off)] {
            assert_eq!(
                sync_experience_policy(&pool, &keys, cfg, 10).await.unwrap(),
                None
            );
        }
        assert!(load_experience_policy(&pool).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn first_boot_seeds_then_unchanged_thresholds_are_a_no_op() {
        let pool = create_test_pool().await;
        let keys = Keys::generate();
        let cfg = with_thresholds(5, 30);

        let first = sync_experience_policy(&pool, &keys, Some(&cfg), 10)
            .await
            .unwrap();
        assert_eq!(first, Some(RecomputeOutcome::default()));
        let seeded = load_experience_policy(&pool).await.unwrap().unwrap();
        assert_eq!(
            (seeded.generation, seeded.min_trades, seeded.min_days),
            (1, 5, 30)
        );

        assert_eq!(
            sync_experience_policy(&pool, &keys, Some(&cfg), 20)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            load_experience_policy(&pool)
                .await
                .unwrap()
                .unwrap()
                .generation,
            1
        );
    }

    #[tokio::test]
    async fn changed_thresholds_recompute_under_a_new_generation() {
        let pool = create_test_pool().await;
        let keys = Keys::generate();
        sync_experience_policy(&pool, &keys, Some(&with_thresholds(5, 30)), 10)
            .await
            .unwrap();

        let out = sync_experience_policy(&pool, &keys, Some(&with_thresholds(3, 7)), 20)
            .await
            .unwrap();

        assert!(out.is_some());
        let policy = load_experience_policy(&pool).await.unwrap().unwrap();
        assert_eq!(
            (policy.generation, policy.min_trades, policy.min_days),
            (2, 3, 7)
        );
    }

    #[tokio::test]
    async fn a_node_key_change_recomputes_even_with_unchanged_thresholds() {
        // History rows are keyed by the node secret: under a new key no
        // declaration can reach them, so the recompute discards them.
        let pool = create_test_pool().await;
        let cfg = with_thresholds(5, 30);
        let old_key = Keys::generate();
        sync_experience_policy(&pool, &old_key, Some(&cfg), 10)
            .await
            .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        bump_history(
            &mut conn,
            &old_key,
            &Keys::generate().public_key().to_string(),
            &hash('a'),
            "cp",
            false,
            1,
            5,
        )
        .await
        .unwrap();
        drop(conn);

        let new_key = Keys::generate();
        let out = sync_experience_policy(&pool, &new_key, Some(&cfg), 20)
            .await
            .unwrap();

        assert_eq!(
            out.map(|o| o.discarded),
            Some(1),
            "recomputed, history discarded"
        );
        let policy = load_experience_policy(&pool).await.unwrap().unwrap();
        assert_eq!(policy.generation, 2);
        assert_eq!(
            policy.node_key_id,
            Some(crate::app::payer::node_key_id(&new_key))
        );
        assert_eq!(
            sync_experience_policy(&pool, &new_key, Some(&cfg), 30)
                .await
                .unwrap(),
            None,
            "same key and thresholds: nothing to do"
        );
    }

    #[test]
    fn cashu_warning_only_when_both_are_enabled() {
        assert!(cashu_conflict_warning(true, true).is_some());
        assert!(cashu_conflict_warning(true, false).is_none());
        assert!(cashu_conflict_warning(false, true).is_none());
        assert!(cashu_conflict_warning(false, false).is_none());
    }
}
