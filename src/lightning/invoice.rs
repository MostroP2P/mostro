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
    use std::env::set_var;
    use std::path::PathBuf;

    use super::is_valid_invoice;
    use crate::{
        error::MostroError,
        settings::{init_global_settings, Settings},
    };

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
        let test_path = PathBuf::from("./");
        set_var("RUN_MODE", "tpl");
        init_global_settings(Settings::new(test_path).unwrap());
        let payment_request = "lnbcrt10n1pjwqagdpp5qwa89czezks35s73fkjspxdssh7h4mmfs4643ey7fgxlng4d3jxqdqqcqzpgxqyz5vqsp5jjlmj6hlq0zxsg5t7n6h6a95ux3ej2w3w2csvdgcpndyvut3aaqs9qyyssqg6py7mmjlcgrscvvq4x3c6kr6f6reqanwkk7rjajm4wepggh4lnku3msrjt3045l0fsl4trh3ctg8ew756wq86mz72mguusey7m0a5qq83t8n6";
        let min_amount_err = is_valid_invoice(payment_request, None, None);
        assert_eq!(Err(MostroError::MinAmountError), min_amount_err);
    }
}
