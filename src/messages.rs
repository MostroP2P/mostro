use crate::models::Order;

pub fn payment_request(order: &Order, hold_invoice: &str) -> String {
    format!(
        "🧌 Somebody wants to buy you {} sats for {} {}.

  Please pay this invoice to start up your selling process, this invoice will expire in 15 minutes.
  
  {}",
        order.amount, order.fiat_code, order.fiat_amount, hold_invoice
    )
}

pub fn waiting_seller_to_pay_invoice(order_id: i64) -> String {
    format!("I have sent a payment request to the seller so he sends your sats for the order Id: {order_id}, as soon as payment is made I will put you both in touch")
}

pub fn buyer_took_order(order: &Order, buyer_pubkey: &str) -> String {
    format!("🧌 Order Id: {}

  {buyer_pubkey} has taken your order and wants to buy your sats. Get in touch and tell him/her how to send you {} {} through {}.

  Once you verify you have received the full amount you have to release the sats", order.id, order.fiat_code, order.fiat_amount, order.payment_method)
}

pub fn get_in_touch_with_seller(order: &Order, seller_pubkey: &str) -> String {
    format!("🧌 Order Id: {}

  Get in touch with the seller, user {seller_pubkey} so as to get the details on how to send the money you must send {} {} through {}.

  Once you send the money, please let me know with the command fiatSent", order.id, order.fiat_code, order.fiat_amount, order.payment_method)
}

pub fn buyer_sentfiat(buyer_pubkey: &str) -> String {
    format!("{buyer_pubkey} has informed that already sent you the fiat money, once you confirmed you received it, please release funds. You will not be able to create another order until you release funds.")
}

pub fn sell_success(buyer_pubkey: &str) -> String {
    format!(
        "Your sale of sats has been completed after confirming payment from {buyer_pubkey} ⚡️🍊⚡️"
    )
}

pub fn funds_released(seller_pubkey: &str) -> String {
    format!("🕐 {seller_pubkey} already released the satoshis, expect your invoice to be paid any time, remember your wallet needs to be online to receive through lighntning network.")
}

pub fn pending_payment_success(amount: i32, order_id: i64, preimage: &str) -> String {
    format!(
        "I have paid your lightning invoice for ${amount} satoshis, Order Id: ${order_id}!

  Proof of payment: ${preimage}"
    )
}

pub fn order_canceled(order_id: i64) -> String {
    format!("Order Id: {order_id} was canceled")
}

pub fn you_sent_fiat(seller_pubkey: &str) -> String {
    format!("🧌 I told  {seller_pubkey} that you have sent fiat money once the seller confirms the money was received, the sats should be sent to you.")
}
