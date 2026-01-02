// Submódulos
pub mod helpers;
pub mod nostr;
pub mod orders;
pub mod pricing;
pub mod queues;
pub mod reputation;

// Re-exportaciones para compatibilidad con imports existentes
// HELPERS
pub use helpers::bytes_to_string;

// PRICING
pub use pricing::{
    calculate_dev_fee, get_bitcoin_price, get_dev_fee, get_expiration_date, get_fee,
    get_market_quote, retries_yadio_request, FiatNames,
};

// ORDERS
pub use orders::{
    get_fiat_amount_requested, get_order, get_tags_for_new_order, get_user_orders_by_id,
    publish_order, set_waiting_invoice_status, update_order_event, validate_invoice,
};

// NOSTR
pub use nostr::{
    connect_nostr, get_keys, get_nostr_client, get_nostr_relays, publish_dev_fee_audit_event,
    send_dm, update_user_rating_event,
};

// QUEUES
pub use queues::{enqueue_cant_do_msg, enqueue_order_msg, enqueue_restore_session_msg};

// REPUTATION
pub use reputation::{get_dispute, notify_taker_reputation, rate_counterpart};

// LIGHTNING operations (now in lightning/operations.rs)
pub use crate::lightning::operations::{
    get_market_amount_and_fee, invoice_subscribe, settle_seller_hold_invoice, show_hold_invoice,
};
