use crate::error::MostroError;
use lightning_invoice::Invoice;
use std::str::FromStr;

/// Decode a lightning invoice (bolt11)
pub fn decode_invoice(payment_request: &str) -> Result<Invoice, MostroError> {
    let invoice = Invoice::from_str(payment_request)?;

    Ok(invoice)
}

/// Verify if an invoice is valid
pub fn is_valid_invoice(payment_request: &str) -> Result<Invoice, MostroError> {
    let invoice = Invoice::from_str(payment_request)?;
    // TODO: Add more validations here

    Ok(invoice)
}
