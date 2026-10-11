//! Issuing reputation attestations (docs/REPUTATION_PORTABILITY.md §5.2,
//! phase 4): what this node attests, and to whom.

use crate::db::completed_trades_for_identity;
use mostro_core::prelude::*;
use mostro_core::user::day_truncate;
use sqlx::SqlitePool;

/// Fewest completed trades an account needs to export its reputation.
pub const MIN_COMPLETED_TRADES: i64 = 10;
/// Fewest ratings received natively an account needs to export.
pub const MIN_NATIVE_REVIEWS: i64 = 5;

/// What an attestation from this node carries for an identity: its native
/// reputation alone, so an import never travels on (§7).
#[derive(Debug, Clone, PartialEq)]
pub struct NativeReputation {
    /// Ratings received here.
    pub reviews: i64,
    /// Their exact average.
    pub rating: f64,
    /// Completed trades here; decides eligibility, not carried.
    pub completed_trades: i64,
    /// UTC day start of the first completed trade here.
    pub since: Option<u64>,
}

impl NativeReputation {
    /// Not banned, at least [`MIN_COMPLETED_TRADES`] completed trades and
    /// [`MIN_NATIVE_REVIEWS`] ratings received, all native, and a first
    /// trade on record.
    pub fn is_eligible(&self, user: &User) -> bool {
        user.is_banned == 0
            && self.completed_trades >= MIN_COMPLETED_TRADES
            && self.reviews >= MIN_NATIVE_REVIEWS
            && self.since.is_some()
    }
}

/// An identity's native reputation on this node, from its user row and its
/// orders.
pub async fn native_reputation(
    pool: &SqlitePool,
    user: &User,
) -> Result<NativeReputation, MostroError> {
    let (reviews, rating) = user.native_stats();
    let (completed_trades, first_trade) = completed_trades_for_identity(pool, &user.pubkey).await?;
    Ok(NativeReputation {
        reviews,
        rating,
        completed_trades,
        since: first_trade.filter(|t| *t > 0).map(day_truncate),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mostro_core::db::Crud;
    use mostro_core::order::Order;
    use mostro_core::reputation::ReputationImport;
    use nostr_sdk::prelude::Keys;
    use uuid::Uuid;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        pool
    }

    async fn order(pool: &SqlitePool, identity: &str, buyer: bool, status: Status, taken_at: i64) {
        let mut order = Order {
            id: Uuid::new_v4(),
            status: status.to_string(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            fiat_code: "USD".to_string(),
            creator_pubkey: Keys::generate().public_key().to_string(),
            amount: 10_000,
            created_at: 1_700_000_000,
            taken_at,
            ..Default::default()
        };
        if buyer {
            order.master_buyer_pubkey = Some(identity.to_string());
        } else {
            order.master_seller_pubkey = Some(identity.to_string());
        }
        order.create(pool).await.unwrap();
    }

    fn rated(identity: &str, ratings: &[u8]) -> User {
        let mut user = User::new(identity.to_string(), 0, 0, 0, 0, 1);
        for rating in ratings {
            user.update_rating(*rating);
        }
        user
    }

    #[tokio::test]
    async fn counts_success_orders_on_either_side_and_dates_the_first() {
        let pool = pool().await;
        let identity = Keys::generate().public_key().to_hex();
        order(&pool, &identity, true, Status::Success, 1_700_090_000).await;
        order(&pool, &identity, false, Status::Success, 0).await;
        order(&pool, &identity, true, Status::Canceled, 1_600_000_000).await;
        order(
            &pool,
            &Keys::generate().public_key().to_hex(),
            true,
            Status::Success,
            1,
        )
        .await;
        let native = native_reputation(&pool, &rated(&identity, &[5]))
            .await
            .unwrap();
        assert_eq!(native.completed_trades, 2);
        // The order without taken_at falls back to its creation, the earlier.
        assert_eq!(native.since, Some(day_truncate(1_700_000_000)));
        assert_eq!((native.reviews, native.rating), (1, 5.0));
    }

    #[tokio::test]
    async fn eligibility_needs_ten_trades_five_native_ratings_and_no_ban() {
        let pool = pool().await;
        let identity = Keys::generate().public_key().to_hex();
        for _ in 0..10 {
            order(&pool, &identity, true, Status::Success, 1_700_000_000).await;
        }
        let user = rated(&identity, &[5, 4, 5, 5, 3]);
        let native = native_reputation(&pool, &user).await.unwrap();
        assert!(native.is_eligible(&user));

        let banned = User {
            is_banned: 1,
            ..user.clone()
        };
        assert!(!native.is_eligible(&banned));
        let four = rated(&identity, &[5, 4, 5, 5]);
        assert!(!native_reputation(&pool, &four)
            .await
            .unwrap()
            .is_eligible(&four));
        let nine = NativeReputation {
            completed_trades: 9,
            ..native.clone()
        };
        assert!(!nine.is_eligible(&user));
    }

    #[tokio::test]
    async fn imported_ratings_never_count_or_travel() {
        let pool = pool().await;
        let identity = Keys::generate().public_key().to_hex();
        for _ in 0..10 {
            order(&pool, &identity, true, Status::Success, 1_700_000_000).await;
        }
        let mut user = rated(&identity, &[5, 5]);
        user.apply_reputation_import(&ReputationImport {
            reviews: 200,
            rating_hundredths: 300,
            since: 1_600_000_000 - 1_600_000_000 % 86_400,
        });
        let native = native_reputation(&pool, &user).await.unwrap();
        assert_eq!((native.reviews, native.rating), (2, 5.0));
        assert!(
            !native.is_eligible(&user),
            "two native ratings are not enough"
        );
        // The imported date is not native either: the first trade here is.
        assert_eq!(native.since, Some(day_truncate(1_700_000_000)));
    }
}
