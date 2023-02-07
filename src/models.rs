use crate::types::{Kind, Status};
use anyhow::{Ok, Result};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use sqlx_crud::SqlxCrud;
use uuid::Uuid;

#[derive(Debug, FromRow, SqlxCrud, Deserialize, Serialize)]
pub struct Order {
    pub id: Uuid,
    pub kind: String,
    pub event_id: String,
    pub hash: Option<String>,
    pub preimage: Option<String>,
    pub buyer_pubkey: Option<String>,
    pub seller_pubkey: Option<String>,
    pub status: String,
    pub prime: i64,
    pub payment_method: String,
    pub amount: i64,
    pub fiat_code: String,
    pub fiat_amount: i64,
    pub buyer_invoice: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct NewOrder {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    pub kind: Kind,
    pub status: Status,
    pub amount: i64,
    pub fiat_code: String,
    pub fiat_amount: i64,
    pub payment_method: String,
    pub prime: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buyer_invoice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
}

#[allow(dead_code)]
impl NewOrder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Option<Uuid>,
        kind: Kind,
        status: Status,
        amount: i64,
        fiat_code: String,
        fiat_amount: i64,
        payment_method: String,
        prime: i64,
        buyer_invoice: Option<String>,
        created_at: Option<i64>,
    ) -> Self {
        Self {
            id,
            kind,
            status,
            amount,
            fiat_code,
            fiat_amount,
            payment_method,
            prime,
            buyer_invoice,
            created_at,
        }
    }
    /// New order from json string
    pub fn from_json(json: &str) -> Result<Self> {
        Ok(serde_json::from_str(json)?)
    }

    /// Get order as json string
    pub fn as_json(&self) -> Result<String> {
        Ok(serde_json::to_string(&self)?)
    }
}
