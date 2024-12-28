use chrono::Utc;
use serde::{Deserialize, Serialize};
#[cfg(feature = "sqlx")]
use sqlx::FromRow;

/// Database representation of an user
#[cfg_attr(feature = "sqlx", derive(FromRow))]
#[derive(Debug, Default, Deserialize, Serialize, Clone, PartialEq)]
pub struct User {
    pub pubkey: String,
    pub is_admin: i64,
    pub is_solver: i64,
    pub is_banned: i64,
    pub category: i64,
    /// We have to be sure that when a user creates a new order (or takes an order),
    /// the trade_index is greater than the one we have in database
    pub last_trade_index: i64,
    pub total_reviews: i64,
    pub total_rating: f64,
    pub last_rating: i64,
    pub max_rating: i64,
    pub min_rating: i64,
    pub created_at: i64,
}

impl User {
    pub fn new(
        pubkey: String,
        is_admin: i64,
        is_solver: i64,
        is_banned: i64,
        category: i64,
        trade_index: i64,
    ) -> Self {
        Self {
            pubkey,
            is_admin,
            is_solver,
            is_banned,
            category,
            last_trade_index: trade_index,
            total_reviews: 0,
            total_rating: 0.0,
            last_rating: 0,
            max_rating: 0,
            min_rating: 0,
            created_at: Utc::now().timestamp(),
        }
    }
}
