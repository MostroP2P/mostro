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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_invoice_description_embeds_order_id_and_fiat_data() {
        // Arrange
        let order_id = "308e1272-d5f4-47e6-bd97-3504baea9c23";
        let fiat_code = "USD";
        let fiat_amount = "100";

        // Act
        let description = hold_invoice_description(order_id, fiat_code, fiat_amount)
            .expect("description building is infallible");

        // Assert
        assert!(description.contains(&format!("Order #{order_id}")));
        assert!(description.contains("SELL BTC for USD 100"));
        assert!(description.starts_with("Escrow amount"));
    }

    #[test]
    fn hold_invoice_description_handles_empty_inputs() {
        // Arrange / Act
        let description =
            hold_invoice_description("", "", "").expect("description building is infallible");

        // Assert: still a well-formed template even with empty fields
        assert!(description.contains("Order #:"));
        assert!(description.contains("SELL BTC for"));
    }
}
