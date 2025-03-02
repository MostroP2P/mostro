use mostro_core::error::MostroError;

pub fn hold_invoice_description(
    order_id: &str,
    fiat_code: &str,
    fiat_amount: &str,
) -> Result<String, MostroError> {
    Ok(format!(
        "Escrow amount Order #{order_id}: SELL BTC for {fiat_code} {fiat_amount} - It WILL FREEZE IN WALLET. It will release once you release. It will return if buyer does not confirm the payment"
    ))
}
