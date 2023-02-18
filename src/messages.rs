use crate::models::Order;
use anyhow::Result;
use nostr_sdk::prelude::*;
use uuid::Uuid;

pub fn payment_request(order: &Order, hold_invoice: &str) -> String {
    format!(
    "🧌 Somebody wants to buy you {} sats for {} {}.

    Please pay this invoice to start up your selling process, this invoice will expire in 15 minutes.

    {}",
        order.amount, order.fiat_code, order.fiat_amount, hold_invoice
    )
}

pub fn waiting_seller_to_pay_invoice(order_id: Uuid) -> String {
    format!("I have sent a payment request to the seller so he sends your sats for the order Id: {order_id}, as soon as payment is made I will put you both in touch")
}

pub fn buyer_took_order(order: &Order, buyer_pubkey: XOnlyPublicKey) -> Result<String> {
    Ok(format!(
        "🧌 Order Id: {}

        {} has taken your order and wants to buy your sats. Get in touch and tell him/her how to send you {} {} through {}.

        Once you verify you have received the full amount you have to release the sats", order.id, buyer_pubkey.to_bech32()?, order.fiat_code, order.fiat_amount, order.payment_method)
    )
}

pub fn get_in_touch_with_seller(order: &Order, seller_pubkey: XOnlyPublicKey) -> Result<String> {
    Ok(format!(
        "🧌 Order Id: {}

        Get in touch with the seller, user {} so as to get the details on how to send the money you must send {} {} through {}.

        Once you send the money, please let me know with the command fiatSent", order.id, seller_pubkey.to_bech32()?, order.fiat_code, order.fiat_amount, order.payment_method)
    )
}

pub fn buyer_sentfiat(buyer_pubkey: XOnlyPublicKey) -> Result<String> {
    Ok(format!("{} has informed that already sent you the fiat money, once you confirmed you received it, please release funds. You will not be able to create another order until you release funds.", buyer_pubkey.to_bech32()?))
}

pub fn sell_success(buyer_pubkey: XOnlyPublicKey) -> Result<String> {
    Ok(format!(
        "Your sale of sats has been completed after confirming payment from {} ⚡️🍊⚡️",
        buyer_pubkey.to_bech32()?
    ))
}

pub fn purchase_completed(seller_pubkey: XOnlyPublicKey) -> Result<String> {
    Ok(format!("
        🪙 Your satoshis purchase has been completed successful, {} has confirmed your fiat payment and I have paid your invoice, enjoy sound money!

        ⚡️🍊⚡️",
        seller_pubkey.to_bech32()?)
    )
}

pub fn funds_released(seller_pubkey: XOnlyPublicKey) -> Result<String> {
    Ok(format!("🕐 {} already released the satoshis, expect your invoice to be paid any time, remember your wallet needs to be online to receive through lighntning network.", seller_pubkey.to_bech32()?))
}

pub fn pending_payment_success(amount: i32, order_id: Uuid, preimage: &str) -> String {
    format!(
        "I have paid your lightning invoice for ${amount} satoshis, Order Id: ${order_id}!

        Proof of payment: ${preimage}"
    )
}

pub fn order_canceled(order_id: Uuid) -> String {
    format!("Order Id: {order_id} was canceled")
}

pub fn you_sent_fiat(seller_pubkey: XOnlyPublicKey) -> Result<String> {
    Ok(format!("🧌 I told {} that you have sent fiat money once the seller confirms the money was received, the sats should be sent to you.", seller_pubkey.to_bech32()?))
}

pub fn invalid_invoice() -> String {
    "Invalid invoice!".to_string()
}

pub fn cant_do() -> String {
    "You can't do that!".to_string()
}

pub fn hold_invoice_description(
    mostro_pubkey: XOnlyPublicKey,
    order_id: &str,
    fiat_code: &str,
    fiat_amount: &str,
) -> Result<String> {
    Ok(format!(
        "{} - Escrow amount Order #{order_id}: SELL BTC for {fiat_code} {fiat_amount} - It WILL FREEZE IN WALLET. It will release once you release. It will return if buyer does not confirm the payment", mostro_pubkey.to_bech32()?
    ))
}
