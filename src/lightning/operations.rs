use crate::config::settings::get_db_pool;
use crate::db;
use crate::flow;
use crate::lightning::LndConnector;
use crate::messages;
use crate::util::helpers::bytes_to_string;
use crate::util::orders::update_order_event;
use crate::util::pricing::{get_fee, get_market_quote};
use crate::util::queues::enqueue_order_msg;
use fedimint_tonic_lnd::lnrpc::invoice::InvoiceState;
use mostro_core::prelude::*;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx_crud::Crud;
use tokio::sync::mpsc::channel;
use tracing::info;

pub async fn show_hold_invoice(
    my_keys: &Keys,
    payment_request: Option<String>,
    buyer_pubkey: &PublicKey,
    seller_pubkey: &PublicKey,
    mut order: Order,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    let mut ln_client = LndConnector::new().await?;
    // Seller pays only the order amount and their Mostro fee
    // Dev fee is NOT charged to seller - it's paid by mostrod from its earnings
    let new_amount = order.amount + order.fee;

    // Now we generate the hold invoice that seller should pay
    let (invoice_response, preimage, hash) = ln_client
        .create_hold_invoice(
            &messages::hold_invoice_description(
                &order.id.to_string(),
                &order.fiat_code,
                &order.fiat_amount.to_string(),
            )
            .map_err(|e| MostroInternalErr(ServiceError::HoldInvoiceError(e.to_string())))?,
            new_amount,
        )
        .await
        .map_err(|e| MostroInternalErr(ServiceError::HoldInvoiceError(e.to_string())))?;
    if let Some(invoice) = payment_request {
        order.buyer_invoice = Some(invoice);
    };

    // Using CRUD to update all fiels
    order.preimage = Some(bytes_to_string(&preimage));
    order.hash = Some(bytes_to_string(&hash));
    order.status = Status::WaitingPayment.to_string();
    order.buyer_pubkey = Some(buyer_pubkey.to_string());
    order.seller_pubkey = Some(seller_pubkey.to_string());

    // We need to publish a new event with the new status
    let pool = db::connect()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let order_updated = update_order_event(my_keys, Status::WaitingPayment, &order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    order_updated
        .update(&pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    let mut new_order = order.as_new_order();
    new_order.status = Some(Status::WaitingPayment);
    new_order.amount = new_amount;

    // We create a Message to send the hold invoice to seller
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::PayInvoice,
        Some(Payload::PaymentRequest(
            Some(new_order),
            invoice_response.payment_request,
            None,
        )),
        *seller_pubkey,
        order.trade_index_seller,
    )
    .await;

    // We notify the buyer (maker) that their order was taken and seller must pay the hold invoice
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::WaitingSellerToPay,
        None,
        *buyer_pubkey,
        order.trade_index_buyer,
    )
    .await;

    let _ = invoice_subscribe(hash, request_id).await;

    Ok(())
}

// Create function to reuse in case of resubscription
pub async fn invoice_subscribe(hash: Vec<u8>, request_id: Option<u64>) -> Result<(), MostroError> {
    let mut ln_client_invoices = LndConnector::new().await?;
    let (tx, mut rx) = channel(100);

    let invoice_task = {
        async move {
            let _ = ln_client_invoices
                .subscribe_invoice(hash, tx)
                .await
                .map_err(|e| e.to_string());
        }
    };
    tokio::spawn(invoice_task);

    // Arc clone db pool to safe use across threads
    let pool = get_db_pool();

    let subs = {
        async move {
            // Receiving msgs from the invoice subscription.
            while let Some(msg) = rx.recv().await {
                let hash = bytes_to_string(msg.hash.as_ref());
                // If this invoice was paid by the seller
                if msg.state == InvoiceState::Accepted {
                    if let Err(e) = flow::hold_invoice_paid(&hash, request_id, &pool).await {
                        info!("Invoice flow error {e}");
                    } else {
                        info!("Invoice with hash {hash} accepted!");
                    }
                } else if msg.state == InvoiceState::Settled {
                    // If the payment was settled
                    if let Err(e) = flow::hold_invoice_settlement(&hash, &pool).await {
                        info!("Invoice flow error {e}");
                    }
                } else if msg.state == InvoiceState::Canceled {
                    // If the payment was canceled
                    if let Err(e) = flow::hold_invoice_canceled(&hash, &pool).await {
                        info!("Invoice flow error {e}");
                    }
                } else {
                    info!("Invoice with hash: {hash} subscribed!");
                }
            }
        }
    };
    tokio::spawn(subs);
    Ok(())
}

pub async fn get_market_amount_and_fee(
    fiat_amount: i64,
    fiat_code: &str,
    premium: i64,
) -> Result<(i64, i64)> {
    // Update amount order
    let new_sats_amount = get_market_quote(&fiat_amount, fiat_code, premium).await?;
    let fee = get_fee(new_sats_amount);

    Ok((new_sats_amount, fee))
}

/// Settle a seller hold invoice
#[allow(clippy::too_many_arguments)]
pub async fn settle_seller_hold_invoice(
    event: &UnwrappedGift,
    ln_client: &mut LndConnector,
    action: Action,
    is_admin: bool,
    order: &Order,
) -> Result<(), MostroError> {
    // Get seller pubkey
    let seller_pubkey = order
        .get_seller_pubkey()
        .map_err(|_| MostroCantDo(CantDoReason::InvalidPubkey))?
        .to_string();
    // Get sender pubkey
    let sender_pubkey = event.rumor.pubkey.to_string();
    // Check if the pubkey is right
    if !is_admin && sender_pubkey != seller_pubkey {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    // Settling the hold invoice
    if let Some(preimage) = order.preimage.as_ref() {
        ln_client.settle_hold_invoice(preimage).await?;
        info!("{action}: Order Id {}: hold invoice settled", order.id);
    } else {
        return Err(MostroCantDo(CantDoReason::InvalidInvoice));
    }
    Ok(())
}
