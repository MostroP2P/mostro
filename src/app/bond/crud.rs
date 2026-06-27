//! SQLite [`Crud`] implementation for [`super::model::Bond`].

use std::future::Future;

use mostro_core::db::Crud;
use sqlx::{query_builder::Separated, Pool, QueryBuilder, Sqlite};

use super::model::Bond;

/// Persisted `bonds` INSERT column names, in bind order. Keep in sync with
/// [`push_bond_insert_binds`], `mostrod` migrations, and [`Bond`]'s
/// `FromRow` mapping. Drift is caught by bond integration tests in `db.rs`.
const BOND_INSERT_COLUMNS: &[&str] = &[
    "id",
    "order_id",
    "parent_bond_id",
    "child_order_id",
    "pubkey",
    "role",
    "amount_sats",
    "slashed_share_sats",
    "state",
    "slashed_reason",
    "hash",
    "preimage",
    "payment_request",
    "payout_invoice",
    "payout_routing_fee_sats",
    "payout_payment_hash",
    "node_share_sats",
    "payout_attempts",
    "invoice_request_attempts",
    "last_invoice_request_at",
    "locked_at",
    "released_at",
    "slashed_at",
    "created_at",
    "taker_identity",
    "taker_trade_index",
    "taker_invoice",
    "taker_fiat_amount",
    "taker_amount",
    "taker_fee",
    "taker_dev_fee",
];

fn push_bond_insert_binds(b: &mut Separated<'_, Sqlite, &'static str>, bond: &Bond) {
    b.push_bind(bond.id)
        .push_bind(bond.order_id)
        .push_bind(bond.parent_bond_id)
        .push_bind(bond.child_order_id)
        .push_bind(&bond.pubkey)
        .push_bind(&bond.role)
        .push_bind(bond.amount_sats)
        .push_bind(bond.slashed_share_sats)
        .push_bind(&bond.state)
        .push_bind(&bond.slashed_reason)
        .push_bind(&bond.hash)
        .push_bind(&bond.preimage)
        .push_bind(&bond.payment_request)
        .push_bind(&bond.payout_invoice)
        .push_bind(bond.payout_routing_fee_sats)
        .push_bind(&bond.payout_payment_hash)
        .push_bind(bond.node_share_sats)
        .push_bind(bond.payout_attempts)
        .push_bind(bond.invoice_request_attempts)
        .push_bind(bond.last_invoice_request_at)
        .push_bind(bond.locked_at)
        .push_bind(bond.released_at)
        .push_bind(bond.slashed_at)
        .push_bind(bond.created_at)
        .push_bind(&bond.taker_identity)
        .push_bind(bond.taker_trade_index)
        .push_bind(&bond.taker_invoice)
        .push_bind(bond.taker_fiat_amount)
        .push_bind(bond.taker_amount)
        .push_bind(bond.taker_fee)
        .push_bind(bond.taker_dev_fee);
}

fn push_bond_update_set(set: &mut Separated<'_, Sqlite, &'static str>, bond: &Bond) {
    set.push("order_id = ").push_bind_unseparated(bond.order_id);
    set.push("parent_bond_id = ")
        .push_bind_unseparated(bond.parent_bond_id);
    set.push("child_order_id = ")
        .push_bind_unseparated(bond.child_order_id);
    set.push("pubkey = ").push_bind_unseparated(&bond.pubkey);
    set.push("role = ").push_bind_unseparated(&bond.role);
    set.push("amount_sats = ")
        .push_bind_unseparated(bond.amount_sats);
    set.push("slashed_share_sats = ")
        .push_bind_unseparated(bond.slashed_share_sats);
    set.push("state = ").push_bind_unseparated(&bond.state);
    set.push("slashed_reason = ")
        .push_bind_unseparated(&bond.slashed_reason);
    set.push("hash = ").push_bind_unseparated(&bond.hash);
    set.push("preimage = ")
        .push_bind_unseparated(&bond.preimage);
    set.push("payment_request = ")
        .push_bind_unseparated(&bond.payment_request);
    set.push("payout_invoice = ")
        .push_bind_unseparated(&bond.payout_invoice);
    set.push("payout_routing_fee_sats = ")
        .push_bind_unseparated(bond.payout_routing_fee_sats);
    set.push("payout_payment_hash = ")
        .push_bind_unseparated(&bond.payout_payment_hash);
    set.push("node_share_sats = ")
        .push_bind_unseparated(bond.node_share_sats);
    set.push("payout_attempts = ")
        .push_bind_unseparated(bond.payout_attempts);
    set.push("invoice_request_attempts = ")
        .push_bind_unseparated(bond.invoice_request_attempts);
    set.push("last_invoice_request_at = ")
        .push_bind_unseparated(bond.last_invoice_request_at);
    set.push("locked_at = ")
        .push_bind_unseparated(bond.locked_at);
    set.push("released_at = ")
        .push_bind_unseparated(bond.released_at);
    set.push("slashed_at = ")
        .push_bind_unseparated(bond.slashed_at);
    set.push("created_at = ")
        .push_bind_unseparated(bond.created_at);
    set.push("taker_identity = ")
        .push_bind_unseparated(&bond.taker_identity);
    set.push("taker_trade_index = ")
        .push_bind_unseparated(bond.taker_trade_index);
    set.push("taker_invoice = ")
        .push_bind_unseparated(&bond.taker_invoice);
    set.push("taker_fiat_amount = ")
        .push_bind_unseparated(bond.taker_fiat_amount);
    set.push("taker_amount = ")
        .push_bind_unseparated(bond.taker_amount);
    set.push("taker_fee = ")
        .push_bind_unseparated(bond.taker_fee);
    set.push("taker_dev_fee = ")
        .push_bind_unseparated(bond.taker_dev_fee);
}

impl Crud for Bond {
    fn create(self, pool: &Pool<Sqlite>) -> impl Future<Output = Result<Self, sqlx::Error>> + Send {
        let pool = pool.clone();
        async move {
            let mut qb = QueryBuilder::new("INSERT INTO bonds (");
            {
                let mut cols = qb.separated(", ");
                for &column in BOND_INSERT_COLUMNS {
                    cols.push(column);
                }
            }
            qb.push(") ");
            qb.push_values(std::iter::once(&self), |mut binds, bond| {
                push_bond_insert_binds(&mut binds, bond);
            });
            qb.push(" RETURNING *");
            qb.build_query_as::<Bond>().fetch_one(&pool).await
        }
    }

    fn update(self, pool: &Pool<Sqlite>) -> impl Future<Output = Result<Self, sqlx::Error>> + Send {
        let pool = pool.clone();
        async move {
            let mut qb = QueryBuilder::new("UPDATE bonds SET ");
            {
                let mut set = qb.separated(", ");
                push_bond_update_set(&mut set, &self);
            }
            qb.push(" WHERE id = ");
            qb.push_bind(self.id);
            qb.push(" RETURNING *");
            qb.build_query_as::<Bond>().fetch_one(&pool).await
        }
    }

    fn by_id(
        pool: &Pool<Sqlite>,
        id: uuid::Uuid,
    ) -> impl Future<Output = Result<Option<Self>, sqlx::Error>> + Send {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, Bond>("SELECT * FROM bonds WHERE id = ? LIMIT 1")
                .bind(id)
                .fetch_optional(&pool)
                .await
        }
    }
}
