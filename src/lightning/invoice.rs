use crate::error::MostroError;
use chrono::prelude::*;
use chrono::Duration;
use dotenvy::var;
use lightning_invoice::{Invoice, SignedRawInvoice};
use std::str::FromStr;

/// Decode a lightning invoice (bolt11)
pub fn decode_invoice(payment_request: &str) -> Result<Invoice, MostroError> {
    let invoice = Invoice::from_str(payment_request)?;

    Ok(invoice)
}

/// Verify if an invoice is valid
pub fn is_valid_invoice(payment_request: &str) -> Result<Invoice, MostroError> {
    let invoice = Invoice::from_str(payment_request)?;
    if invoice.is_expired() {
        return Err(MostroError::InvoiceExpiredError);
    }
    let parsed = payment_request.parse::<SignedRawInvoice>()?;

    let (parsed_invoice, _, _) = parsed.into_parts();

    let expiration_window = var("INVOICE_EXPIRATION_WINDOW")
        .expect("INVOICE_EXPIRATION_WINDOW is not set")
        .parse::<i64>()?;
    let latest_date = Utc::now() + Duration::seconds(expiration_window);
    let latest_date = latest_date.timestamp() as u64;
    let expires_at =
        invoice.expiry_time().as_secs() + parsed_invoice.data.timestamp.as_unix_timestamp();

    if expires_at < latest_date {
        return Err(MostroError::MinExpirationTimeError);
    }
    let min_payment_amount = var("MIN_PAYMENT_AMT")
        .expect("INVOICE_EXPIRATION_WINDOW is not set")
        .parse::<u64>()?;

    let amount_msat = invoice.amount_milli_satoshis().unwrap_or(0) / 1000;
    if amount_msat > 0 && amount_msat < min_payment_amount {
        return Err(MostroError::MinAmountError);
    }

    Ok(invoice)
}
