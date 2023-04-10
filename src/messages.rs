use anyhow::Result;
use mostro_core::order::Order;
use nostr_sdk::prelude::*;
use uuid::Uuid;

// Not being used yet
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

pub fn buyer_sentfiat(order_id: Uuid, buyer_pubkey: XOnlyPublicKey) -> Result<String> {
    Ok(format!(
    "🧌 Order Id: {}

    {} has informed that already sent you the fiat money, once you confirmed you received it, please release funds. You will not be able to create another order until you release funds.",
    order_id,
    buyer_pubkey.to_bech32()?))
}

pub fn sell_success(order_id: Uuid, buyer_pubkey: XOnlyPublicKey) -> Result<String> {
    Ok(format!(
        "🧌 Order Id: {}

        Your sale of sats has been completed after confirming payment from {} ⚡️🍊⚡️",
        order_id,
        buyer_pubkey.to_bech32()?
    ))
}

pub fn purchase_completed(order_id: Uuid, seller_pubkey: XOnlyPublicKey) -> Result<String> {
    Ok(format!(
        "🧌 Order Id: {}

        🪙 Your satoshis purchase has been completed successful, {} has confirmed your fiat payment and I have paid your invoice, enjoy sound money!

        ⚡️🍊⚡️",
        order_id,
        seller_pubkey.to_bech32()?)
    )
}

pub fn funds_released(order_id: Uuid, seller_pubkey: XOnlyPublicKey) -> Result<String> {
    Ok(format!("🧌 Order Id: {}

    🕐 {} already released the satoshis, expect your invoice to be paid any time, remember your wallet needs to be online to receive through lighntning network.",
    order_id,
    seller_pubkey.to_bech32()?))
}

// Not being used yet
pub fn pending_payment_success(amount: i32, order_id: Uuid, preimage: &str) -> String {
    format!(
        "I have paid your lightning invoice for ${amount} satoshis, Order Id: ${order_id}!

        Proof of payment: ${preimage}"
    )
}

pub fn order_canceled(order_id: Uuid) -> String {
    format!("Order Id: {order_id} was canceled")
}

pub fn you_sent_fiat(order_id: Uuid, seller_pubkey: XOnlyPublicKey) -> Result<String> {
    Ok(format!(
    "🧌 Order Id: {}

    I told {} that you have sent fiat money once the seller confirms the money was received, the sats should be sent to you.",
    order_id,
    seller_pubkey.to_bech32()?))
}

// Not being used yet
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

pub fn send_sell_request_invoice_req_market_price(
    order_id: Uuid,
    sats: i64,
    prime: i64,
) -> Result<String> {
    Ok(format!(
            "🧌 Order Id: {}
    
        create a lightning invoice of {} sats , this value is calculated as market price added with requested premiun ( {}% ).
        
        Use again takesell command like this:
        
        takesell --order orderid --invoice invoice_string",
            order_id, sats, prime
        ))
}

pub fn send_buy_request_invoice_req_market_price(
    order_id: Uuid,
    sats: i64,
    _prime: i64,
) -> Result<String> {
    Ok(format!(
            "We sent a hold invoice to the seller of order id : {} create a lightning invoice of {} sats to proceed.
            
            Check with getdm command when hold invoice is paid by seller before proceding."
            , order_id, sats

        ))
}
