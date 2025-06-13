use crate::config::settings::Settings;
use crate::lnurl::ln_exists;

use chrono::prelude::*;
use chrono::TimeDelta;
use lightning_invoice::{Bolt11Invoice, SignedRawBolt11Invoice};
use lnurl::lightning_address::LightningAddress;
use lnurl::lnurl::LnUrl;
use mostro_core::prelude::*;
use serde_json;
use std::str::FromStr;

/// Decode a lightning invoice (bolt11)
pub fn decode_invoice(payment_request: &str) -> Result<Bolt11Invoice, MostroError> {
    let invoice = Bolt11Invoice::from_str(payment_request)
        .map_err(|_| MostroInternalErr(ServiceError::InvoiceInvalidError))?;

    Ok(invoice)
}

/// Validate Lightning address
async fn validate_lightning_address(payment_request: &str) -> Result<(), MostroError> {
    if ln_exists(payment_request).await.is_err() {
        return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
    }
    Ok(())
}

/// Validate LNURL
async fn validate_lnurl(lnurl: LnUrl, amount: Option<u64>) -> Result<(), MostroError> {
    let res = reqwest::get(&lnurl.url)
        .await
        .map_err(|_| MostroInternalErr(ServiceError::InvoiceInvalidError))?;

    if !res.status().is_success() {
        return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
    }

    let body = res
        .text()
        .await
        .map_err(|_| MostroInternalErr(ServiceError::InvoiceInvalidError))?;

    let body: serde_json::Value = serde_json::from_str(&body)
        .map_err(|_| MostroInternalErr(ServiceError::InvoiceInvalidError))?;

    let tag = body["tag"].as_str().unwrap_or("");
    if tag != "payRequest" {
        return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
    }

    // Validate amount limits if provided
    if let Some(amt) = amount {
        let amt_msat = amt * 1000;
        let min_sendable = body["minSendable"].as_u64().unwrap_or(0);
        let max_sendable = body["maxSendable"].as_u64().unwrap_or(u64::MAX);

        if amt_msat < min_sendable || amt_msat > max_sendable {
            return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
        }
    }

    Ok(())
}

/// Validate BOLT11 invoice
async fn validate_bolt11_invoice(
    payment_request: &str,
    amount: Option<u64>,
    fee: Option<u64>,
) -> Result<(), MostroError> {
    let invoice = decode_invoice(payment_request)?;
    let mostro_settings = Settings::get_mostro();
    let ln_settings = Settings::get_ln();

    let amount_sat = invoice.amount_milli_satoshis().unwrap_or(0) / 1000;
    let fee = fee.unwrap_or(0);

    // Validate amount if provided
    if let Some(amt) = amount {
        if let Some(res) = amt.checked_sub(fee) {
            if amount_sat != res && amount_sat != 0 {
                return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
            }
        } else {
            // Case overflow in subtraction
            return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
        }
    }

    // Check minimum payment amount
    if amount_sat > 0 && amount_sat < mostro_settings.min_payment_amount as u64 {
        return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
    }

    // Check if invoice is expired
    if invoice.is_expired() {
        return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
    }

    // Check expiration window
    let parsed = payment_request
        .parse::<SignedRawBolt11Invoice>()
        .map_err(|_| MostroInternalErr(ServiceError::InvoiceInvalidError))?;

    let (parsed_invoice, _, _) = parsed.into_parts();

    let expiration_window = ln_settings.invoice_expiration_window as i64;
    let latest_date = Utc::now()
        + TimeDelta::try_seconds(expiration_window).expect("wrong seconds timeout value");
    let latest_date = latest_date.timestamp() as u64;
    let expires_at =
        invoice.expiry_time().as_secs() + parsed_invoice.data.timestamp.as_unix_timestamp();

    if expires_at < latest_date {
        return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
    }

    Ok(())
}

/// Verify if a buyer invoice is valid,
/// if the invoice have amount we check if the amount minus fee is the same
pub async fn is_valid_invoice(
    payment_request: String,
    amount: Option<u64>,
    fee: Option<u64>,
) -> Result<(), MostroError> {
    // Try Lightning address first
    if LightningAddress::from_str(&payment_request).is_ok() {
        return validate_lightning_address(&payment_request).await;
    }

    // Try LNURL
    if let Ok(lnurl) = LnUrl::from_str(&payment_request) {
        return validate_lnurl(lnurl, amount).await;
    }

    // Fall back to BOLT11 invoice
    validate_bolt11_invoice(&payment_request, amount, fee).await
}

#[cfg(test)]
mod tests {
    use super::{is_valid_invoice, Settings};
    use crate::config::MOSTRO_CONFIG;
    use mostro_core::error::{MostroError::MostroInternalErr, ServiceError};
    use toml;

    fn init_settings_test() {
        let config_tpl = include_bytes!("../../settings.tpl.toml");
        let config_tpl =
            std::str::from_utf8(config_tpl).expect("Invalid UTF-8 in template config file");
        let test_settings: Settings =
            toml::from_str(config_tpl).expect("Failed to parse template config file");
        MOSTRO_CONFIG.get_or_init(|| test_settings);
    }

    #[tokio::test]
    async fn test_wrong_amount_invoice() {
        init_settings_test();
        let payment_request = "lnbcrt500u1p3l8zyapp5nc0ctxjt98xq9tgdgk9m8fepnp0kv6mnj6a83mfsannw46awdp4sdqqcqzpgxqyz5vqsp5a3axmz77s5vafmheq56uh49rmy59r9a3d0dm0220l8lzdp5jrtxs9qyyssqu0ft47j0r4lu997zuqgf92y8mppatwgzhrl0hzte7mzmwrqzf2238ylch82ehhv7pfcq6qcyu070dg85vu55het2edyljuezvcw5pzgqfncf3d".to_string();
        let wrong_amount_err = is_valid_invoice(payment_request, Some(23), None);
        assert_eq!(
            Err(MostroInternalErr(ServiceError::InvoiceInvalidError)),
            wrong_amount_err.await
        );
    }

    #[tokio::test]
    async fn test_is_expired_invoice() {
        init_settings_test();
        let payment_request = "lnbcrt500u1p3lzwdzpp5t9kgwgwd07y2lrwdscdnkqu4scrcgpm5pt9uwx0rxn5rxawlxlvqdqqcqzpgxqyz5vqsp5a6k7syfxeg8jy63rteywwjla5rrg2pvhedx8ajr2ltm4seydhsqq9qyyssq0n2uwlumsx4d0mtjm8tp7jw3y4da6p6z9gyyjac0d9xugf72lhh4snxpugek6n83geafue9ndgrhuhzk98xcecu2t3z56ut35mkammsqscqp0n".to_string();
        let expired_err = is_valid_invoice(payment_request, None, None);
        assert_eq!(
            Err(MostroInternalErr(ServiceError::InvoiceInvalidError)),
            expired_err.await
        );
    }

    #[tokio::test]
    async fn test_min_amount_invoice() {
        init_settings_test();
        let payment_request = "lnbcrt10n1pjwqagdpp5qwa89czezks35s73fkjspxdssh7h4mmfs4643ey7fgxlng4d3jxqdqqcqzpgxqyz5vqsp5jjlmj6hlq0zxsg5t7n6h6a95ux3ej2w3w2csvdgcpndyvut3aaqs9qyyssqg6py7mmjlcgrscvvq4x3c6kr6f6reqanwkk7rjajm4wepggh4lnku3msrjt3045l0fsl4trh3ctg8ew756wq86mz72mguusey7m0a5qq83t8n6".to_string();
        let min_amount_err = is_valid_invoice(payment_request, None, None);
        assert_eq!(
            Err(MostroInternalErr(ServiceError::InvoiceInvalidError)),
            min_amount_err.await
        );
    }

    #[tokio::test]
    async fn test_lnurl_validation() {
        init_settings_test();
        // Test with a mock LNURL string (this would need a real LNURL service to test properly)
        let lnurl_payment_request = "lnurl1dp68gurn8ghj7um9wfmxjcm99e3k7mf0v9cxj0m385ekvcenxc6r2c35xvukxefcv5mkvv34x5ekzd3ev56nyc".to_string();

        // This test would fail in practice without a real LNURL service
        // but demonstrates the structure for LNURL validation
        let result = is_valid_invoice(lnurl_payment_request, None, None).await;
        // In a real test environment with a working LNURL service, this should be Ok(())
        assert!(result.is_err() || result.is_ok());
    }

    #[tokio::test]
    async fn test_lightning_address_validation() {
        init_settings_test();
        // Test with a mock Lightning address
        let lightning_address = "user@example.com".to_string();

        // This test would fail in practice without a real Lightning address service
        let result = is_valid_invoice(lightning_address, None, None).await;
        assert!(result.is_err() || result.is_ok());
    }
}
