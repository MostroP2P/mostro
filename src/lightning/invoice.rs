use crate::error::MostroError;
use crate::settings::Settings;

use chrono::prelude::*;
use chrono::Duration;
use lightning_invoice::{Invoice, SignedRawInvoice};
use std::str::FromStr;

/// Decode a lightning invoice (bolt11)
pub fn decode_invoice(payment_request: &str) -> Result<Invoice, MostroError> {
    let invoice = Invoice::from_str(payment_request)?;

    Ok(invoice)
}

/// Verify if a buyer invoice is valid
pub fn is_valid_invoice(
    payment_request: &str,
    amount: Option<u64>,
    fee: Option<u64>,
) -> Result<Invoice, MostroError> {
    let invoice = Invoice::from_str(payment_request)?;
    let mostro_settings = Settings::get_mostro();
    let ln_settings = Settings::get_ln();

    let amount_msat = invoice.amount_milli_satoshis().unwrap_or(0) / 1000;
    let fee = fee.unwrap_or(0);

    if let Some(amt) = amount {
        if amount_msat > 0 && amount_msat != (amt - fee) {
            return Err(MostroError::WrongAmountError);
        }
    }
    if amount_msat > 0 && amount_msat < mostro_settings.min_payment_amount as u64 {
        return Err(MostroError::MinAmountError);
    }
    if invoice.is_expired() {
        return Err(MostroError::InvoiceExpiredError);
    }
    let parsed = payment_request.parse::<SignedRawInvoice>()?;

    let (parsed_invoice, _, _) = parsed.into_parts();

    let expiration_window = ln_settings.invoice_expiration_window as i64;
    let latest_date = Utc::now() + Duration::seconds(expiration_window);
    let latest_date = latest_date.timestamp() as u64;
    let expires_at =
        invoice.expiry_time().as_secs() + parsed_invoice.data.timestamp.as_unix_timestamp();

    if expires_at < latest_date {
        return Err(MostroError::MinExpirationTimeError);
    }

    Ok(invoice)
}

#[cfg(test)]
mod tests {
    use super::is_valid_invoice;
    use crate::error::MostroError;

    #[test]
    fn test_wrong_amount_invoice() {
        let payment_request = "lnbcrt500u1p3l8zyapp5nc0ctxjt98xq9tgdgk9m8fepnp0kv6mnj6a83mfsannw46awdp4sdqqcqzpgxqyz5vqsp5a3axmz77s5vafmheq56uh49rmy59r9a3d0dm0220l8lzdp5jrtxs9qyyssqu0ft47j0r4lu997zuqgf92y8mppatwgzhrl0hzte7mzmwrqzf2238ylch82ehhv7pfcq6qcyu070dg85vu55het2edyljuezvcw5pzgqfncf3d";
        let wrong_amount_err = is_valid_invoice(payment_request, Some(23), None);
        assert_eq!(Err(MostroError::WrongAmountError), wrong_amount_err);
    }

    #[test]
    fn test_is_expired_invoice() {
        let payment_request = "lnbcrt500u1p3lzwdzpp5t9kgwgwd07y2lrwdscdnkqu4scrcgpm5pt9uwx0rxn5rxawlxlvqdqqcqzpgxqyz5vqsp5a6k7syfxeg8jy63rteywwjla5rrg2pvhedx8ajr2ltm4seydhsqq9qyyssq0n2uwlumsx4d0mtjm8tp7jw3y4da6p6z9gyyjac0d9xugf72lhh4snxpugek6n83geafue9ndgrhuhzk98xcecu2t3z56ut35mkammsqscqp0n";
        let expired_err = is_valid_invoice(payment_request, None, None);
        assert_eq!(Err(MostroError::InvoiceExpiredError), expired_err);
    }

    #[test]
    fn test_min_amount_invoice() {
        let payment_request = "lnbcrt10n1p3l8ysvpp5scf3rd8e8j2f9k7qktfjmpqr4xazj5dr5ygp84wa22sen3wxcevsdqqcqzpgxqyz5vqsp55wp60pzn4889l56538zt7jcr2sgag4xreen3yuzpudlmac3acqls9qyyssqu8rmewmly2xyuqn03vttwsysnnelr0thjstavk2qu6ygs7ampe08h74u9a7qlkuudagpy6mc06gz6qgmq3x582u54rd8gdx3nfvxmlqqrttwdj";
        let min_amount_err = is_valid_invoice(payment_request, None, None);
        assert_eq!(Err(MostroError::MinAmountError), min_amount_err);
    }
}
