use crate::db::connect;
use crate::util::send_message_sync;
use crate::{db::Order, lightning::is_valid_invoice};
use anyhow::Result;
use lnurl::lightning_address::LightningAddress;
use mostro_core::message::{Action, Message, Payload};
use mostro_core::order::Status;
use nostr_sdk::prelude::*;
use std::str::FromStr;
use uuid::Uuid;

pub async fn execute_add_invoice(
    order_id: &Uuid,
    invoice: &str,
    identity_keys: &Keys,
    mostro_key: PublicKey,
    client: &Client,
) -> Result<()> {
    let pool = connect().await?;
    let mut order = Order::get_by_id(&pool, &order_id.to_string()).await?;
    let trade_keys = order
        .trade_keys
        .clone()
        .ok_or(anyhow::anyhow!("Missing trade keys"))?;
    let trade_keys = Keys::parse(&trade_keys)?;

    println!(
        "Sending a lightning invoice {} to mostro pubId {}",
        order_id, mostro_key
    );
    // Check invoice string
    let ln_addr = LightningAddress::from_str(invoice);
    let payload = if ln_addr.is_ok() {
        Some(Payload::PaymentRequest(None, invoice.to_string(), None))
    } else {
        match is_valid_invoice(invoice) {
            Ok(i) => Some(Payload::PaymentRequest(None, i.to_string(), None)),
            Err(e) => {
                println!("Invalid invoice: {}", e);
                None
            }
        }
    };
    let request_id = Uuid::new_v4().as_u128() as u64;
    // Create AddInvoice message
    let add_invoice_message = Message::new_order(
        Some(*order_id),
        Some(request_id),
        None,
        Action::AddInvoice,
        payload,
    );

    let dm = send_message_sync(
        client,
        Some(identity_keys),
        &trade_keys,
        mostro_key,
        add_invoice_message,
        true,
        false,
    )
    .await?;

    dm.iter().for_each(|el| {
        let message = el.0.get_inner_message_kind();
        if message.request_id == Some(request_id) && message.action == Action::WaitingSellerToPay {
            println!("Now we should wait for the seller to pay the invoice");
        }
    });
    match order
        .set_status(Status::WaitingPayment.to_string())
        .save(&pool)
        .await
    {
        Ok(_) => println!("Order status updated"),
        Err(e) => println!("Failed to update order status: {}", e),
    }

    Ok(())
}
