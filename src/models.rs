use sqlx::FromRow;
use sqlx_crud::SqlxCrud;

#[derive(Debug, FromRow, SqlxCrud)]
pub struct Order {
    pub id: i64,
    pub kind: String,
    pub event_id: String,
    pub event_kind: i64,
    pub hash: Option<String>,
    pub preimage: Option<String>,
    pub buyer_pubkey: Option<String>,
    pub seller_pubkey: Option<String>,
    pub status: String,
    pub description: String,
    pub payment_method: String,
    pub amount: i64,
    pub fiat_code: String,
    pub fiat_amount: i64,
    pub buyer_invoice: Option<String>,
    pub created_at: chrono::naive::NaiveDateTime,
}
