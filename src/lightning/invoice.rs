use crate::cli::settings::Settings;
use crate::error::MostroError;
use crate::lnurl::ln_exists;

use chrono::prelude::*;
use chrono::Duration;
use lightning_invoice::{Bolt11Invoice, SignedRawBolt11Invoice};
use lnurl::lightning_address::LightningAddress;
use std::str::FromStr;

/// Decode a lightning invoice (bolt11)
pub fn decode_invoice(payment_request: &str) -> Result<Bolt11Invoice, MostroError> {
    let invoice = Bolt11Invoice::from_str(payment_request)?;

    Ok(invoice)
}

/// Verify if a buyer invoice is valid,
/// if the invoice have amount we check if the amount minus fee is the same
pub async fn is_valid_invoice(
    payment_request: String,
    amount: Option<u64>,
    fee: Option<u64>,
) -> Result<(), MostroError> {
    // Check if it's a lightning address
    let ln_addr = LightningAddress::from_str(&payment_request);
    // Is it a ln address
    if ln_addr.is_ok() {
        if amount != Some(0) || ln_exists(&payment_request).await.is_err() {
            return Err(MostroError::ParsingInvoiceError);
        }
    } else {
        let invoice = decode_invoice(&payment_request)?;
        let mostro_settings = Settings::get_mostro();
        let ln_settings = Settings::get_ln();

        let amount_sat = invoice.amount_milli_satoshis().unwrap_or(0) / 1000;

        let fee = fee.unwrap_or(0);

        if let Some(amt) = amount {
            if amount_sat > 0 && amount_sat != (amt - fee) {
                return Err(MostroError::WrongAmountError);
            }
        }

        if amount_sat > 0 && amount_sat < mostro_settings.min_payment_amount as u64 {
            return Err(MostroError::MinAmountError);
        }

        if invoice.is_expired() {
            return Err(MostroError::InvoiceExpiredError);
        }

        let parsed = payment_request.parse::<SignedRawBolt11Invoice>()?;

        let (parsed_invoice, _, _) = parsed.into_parts();

        let expiration_window = ln_settings.invoice_expiration_window as i64;
        let latest_date = Utc::now() + Duration::seconds(expiration_window);
        let latest_date = latest_date.timestamp() as u64;
        let expires_at =
            invoice.expiry_time().as_secs() + parsed_invoice.data.timestamp.as_unix_timestamp();

        if expires_at < latest_date {
            return Err(MostroError::MinExpirationTimeError);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::env::set_var;
    use std::path::PathBuf;

    use super::is_valid_invoice;
    use crate::{cli::settings::Settings, error::MostroError, MOSTRO_CONFIG};

    fn init_settings_test() {
        let test_path = PathBuf::from("./");
        set_var("RUN_MODE", "tpl");
        MOSTRO_CONFIG.get_or_init(|| Settings::new(test_path).unwrap());
    }

    #[tokio::test]
    async fn test_wrong_amount_invoice() {
        init_settings_test();
        let payment_request = "lnbcrt500u1p3l8zyapp5nc0ctxjt98xq9tgdgk9m8fepnp0kv6mnj6a83mfsannw46awdp4sdqqcqzpgxqyz5vqsp5a3axmz77s5vafmheq56uh49rmy59r9a3d0dm0220l8lzdp5jrtxs9qyyssqu0ft47j0r4lu997zuqgf92y8mppatwgzhrl0hzte7mzmwrqzf2238ylch82ehhv7pfcq6qcyu070dg85vu55het2edyljuezvcw5pzgqfncf3d".to_string();
        let wrong_amount_err = is_valid_invoice(payment_request, Some(23), None);
        assert_eq!(Err(MostroError::WrongAmountError), wrong_amount_err.await);
    }

    #[tokio::test]
    async fn test_is_expired_invoice() {
        init_settings_test();
        let payment_request = "lnbcrt500u1p3lzwdzpp5t9kgwgwd07y2lrwdscdnkqu4scrcgpm5pt9uwx0rxn5rxawlxlvqdqqcqzpgxqyz5vqsp5a6k7syfxeg8jy63rteywwjla5rrg2pvhedx8ajr2ltm4seydhsqq9qyyssq0n2uwlumsx4d0mtjm8tp7jw3y4da6p6z9gyyjac0d9xugf72lhh4snxpugek6n83geafue9ndgrhuhzk98xcecu2t3z56ut35mkammsqscqp0n".to_string();
        let expired_err = is_valid_invoice(payment_request, None, None);
        assert_eq!(Err(MostroError::InvoiceExpiredError), expired_err.await);
    }

    #[tokio::test]
    async fn test_min_amount_invoice() {
        init_settings_test();
        let payment_request = "lnbcrt10n1pjwqagdpp5qwa89czezks35s73fkjspxdssh7h4mmfs4643ey7fgxlng4d3jxqdqqcqzpgxqyz5vqsp5jjlmj6hlq0zxsg5t7n6h6a95ux3ej2w3w2csvdgcpndyvut3aaqs9qyyssqg6py7mmjlcgrscvvq4x3c6kr6f6reqanwkk7rjajm4wepggh4lnku3msrjt3045l0fsl4trh3ctg8ew756wq86mz72mguusey7m0a5qq83t8n6".to_string();
        let min_amount_err = is_valid_invoice(payment_request, None, None);
        assert_eq!(Err(MostroError::MinAmountError), min_amount_err.await);
    }
}
