use crate::config::settings::Settings;
use crate::lnurl::ln_exists;

use chrono::prelude::*;
use chrono::TimeDelta;
use lightning_invoice::{Bolt11Invoice, SignedRawBolt11Invoice};
use lnurl::lightning_address::LightningAddress;
use lnurl::lnurl::LnUrl;
use mostro_core::prelude::*;
use std::str::FromStr;

/// Decodes a BOLT11 Lightning invoice from its string representation.
///
/// This function parses a Lightning Network payment request string and returns
/// a structured `Bolt11Invoice` object that can be used to extract invoice details
/// such as amount, description, expiration time, and payment hash.
///
/// # Arguments
///
/// * `payment_request` - A string slice containing the BOLT11 invoice to decode
///
/// # Returns
///
/// * `Ok(Bolt11Invoice)` - Successfully decoded invoice
/// * `Err(MostroError)` - If the invoice string is malformed or invalid
///
/// # Examples
///
/// ```ignore
/// let invoice_str = "lnbc1pvjluezpp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypq...";
/// let invoice = decode_invoice(invoice_str)?;
/// ```
pub fn decode_invoice(payment_request: &str) -> Result<Bolt11Invoice, MostroError> {
    let invoice = Bolt11Invoice::from_str(payment_request)
        .map_err(|_| MostroInternalErr(ServiceError::InvoiceInvalidError))?;

    Ok(invoice)
}

/// Validates a Lightning Address by checking if it exists and is reachable.
///
/// Lightning Addresses are human-readable identifiers (similar to email addresses)
/// that resolve to Lightning payment endpoints. This function verifies that the
/// address is properly formatted and that the underlying service is accessible.
///
/// # Arguments
///
/// * `payment_request` - A string slice containing the Lightning Address (e.g., "user@domain.com")
///
/// # Returns
///
/// * `Ok(())` - If the Lightning Address is valid and reachable
/// * `Err(MostroError)` - If the address is invalid, malformed, or unreachable
///
/// # Notes
///
/// This function performs a network request to validate the address, so it may
/// fail due to network issues even if the address format is correct.
async fn validate_lightning_address(payment_request: &str) -> Result<(), MostroError> {
    if ln_exists(payment_request).await.is_err() {
        return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
    }
    Ok(())
}

/// Validates a BOLT11 Lightning invoice with comprehensive checks.
///
/// This function performs thorough validation of a BOLT11 invoice including:
/// - Amount verification against expected values and fees
/// - Minimum payment amount enforcement
/// - Expiration time validation
/// - Invoice expiration window compliance
///
/// # Arguments
///
/// * `payment_request` - The BOLT11 invoice string to validate
/// * `amount` - Optional expected amount in satoshis (before fees)
/// * `fee` - Optional fee amount in satoshis to subtract from expected amount
///
/// # Returns
///
/// * `Ok(())` - If all validation checks pass
/// * `Err(MostroError)` - If any validation check fails
///
/// # Validation Rules
///
/// - If `amount` is provided, the invoice amount must match `amount - fee`
/// - Invoice amount must meet minimum payment threshold (if non-zero)
/// - Invoice must not be expired
/// - Invoice expiration must be within acceptable time window
///
/// # Notes
///
/// Zero-amount invoices are allowed but still subject to expiration checks.
/// The function uses configuration settings for minimum amounts and time windows.
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
        if let Some(expected_sats_amount) = amt.checked_sub(fee) {
            if amount_sat != expected_sats_amount && amount_sat != 0 {
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

/// Validates a payment request, automatically detecting and handling different formats.
///
/// This is the main validation function that accepts various Lightning payment formats
/// and routes them to the appropriate validation logic. It supports:
/// - Lightning Addresses (user@domain.com format)
/// - LNURL-pay requests (lnurl1... format)
/// - BOLT11 invoices (lnbc... format)
///
/// # Arguments
///
/// * `payment_request` - The payment request string in any supported format
/// * `amount` - Optional expected amount in satoshis for validation
/// * `fee` - Optional fee amount in satoshis (only used for BOLT11 invoices)
///
/// # Returns
///
/// * `Ok(())` - If the payment request is valid and passes all checks
/// * `Err(MostroError)` - If validation fails for any reason
///
/// # Format Detection
///
/// The function tries to parse the payment request in the following order:
/// 1. Lightning Address - if it matches email-like format
/// 2. LNURL - if it can be parsed as a valid LNURL
/// 3. BOLT11 - falls back to BOLT11 invoice validation
///
/// # Usage
///
/// This function is typically used to validate buyer invoices in trading contexts
/// where the exact payment format may vary depending on user preference.
pub async fn is_valid_invoice(
    payment_request: String,
    amount: Option<u64>,
    fee: Option<u64>,
) -> Result<(), MostroError> {
    // Try Lightning address or LNURL first
    if LightningAddress::from_str(&payment_request).is_ok()
        || LnUrl::from_str(&payment_request).is_ok()
    {
        return validate_lightning_address(&payment_request).await;
    }

    // Fall back to BOLT11 invoice
    validate_bolt11_invoice(&payment_request, amount, fee).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MOSTRO_CONFIG;
    use axum::{http::StatusCode, routing::get, Json, Router};
    use bech32::{encode, ToBase32};
    use mostro_core::error::{MostroError::MostroInternalErr, ServiceError};
    use serde_json::json;
    use tokio::net::TcpListener;
    use toml;

    fn init_settings_test() {
        let config_tpl = include_bytes!("../../settings.tpl.toml");
        let config_tpl =
            std::str::from_utf8(config_tpl).expect("Invalid UTF-8 in template config file");
        let test_settings: Settings =
            toml::from_str(config_tpl).expect("Failed to parse template config file");
        MOSTRO_CONFIG.get_or_init(|| test_settings);
    }

    async fn handle_request() -> (StatusCode, Json<serde_json::Value>) {
        let response = json!({
            "status": "OK",
            "tag": "payRequest",
            "callback": "http://localhost:8080/callback",
            "minSendable": 1000,
            "maxSendable": 10000000,
            "metadata": "[[\"text/plain\",\"Test payment\"]]",
            "pr": "lnbcrt500u1p3l8zyapp5nc0ctxjt98xq9tgdgk9m8fepnp0kv6mnj6a83mfsannw46awdp4sdqqcqzpgxqyz5vqsp5a3axmz77s5vafmheq56uh49rmy59r9a3d0dm0220l8lzdp5jrtxs9qyyssqu0ft47j0r4lu997zuqgf92y8mppatwgzhrl0hzte7mzmwrqzf2238ylch82ehhv7pfcq6qcyu070dg85vu55het2edyljuezvcw5pzgqfncf3d"
        });

        (StatusCode::OK, Json(response))
    }

    // Helper function to start test server
    async fn start_test_server() -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/.well-known/lnurlp/MostroP2P", get(handle_request))
            .route(
                "/.well-known/lnurlp/MostroP2Ptestlnurl",
                get(handle_request),
            )
            .layer(tower_http::cors::CorsLayer::permissive());

        let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();

        let server = axum::serve(listener, app);
        let handle = tokio::spawn(async move {
            server.await.unwrap();
        });

        // Build the LNURL for this port and encode it
        let url = format!(
            "http://localhost:{}/.well-known/lnurlp/MostroP2Ptestlnurl",
            port
        );
        let lnurl = encode("lnurl", url.as_bytes().to_base32(), bech32::Variant::Bech32).unwrap();

        (lnurl, handle)
    }

    #[tokio::test]
    async fn test_wrong_amount_invoice() {
        init_settings_test();
        let payment_request = "lnbcrt500u1p3lzwdzpp5t9kgwgwd07y2lrwdscdnkqu4scrcgpm5pt9uwx0rxn5rxawlxlvqdqqcqzpgxqyz5vqsp5a6k7syfxeg8jy63rteywwjla5rrg2pvhedx8ajr2ltm4seydhsqq9qyyssq0n2uwlumsx4d0mtjm8tp7jw3y4da6p6z9gyyjac0d9xugf72lhh4snxpugek6n83geafue9ndgrhuhzk98xcecu2t3z56ut35mkammsqscqp0n".to_string();
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
    async fn test_zero_amount_invoice() {
        init_settings_test();
        let payment_request = "lnbc1p583aqqpp5gc52tl8ycp96jwurmnxwhz537vlancxxaxk92r9fl6f77z3r8q7qdq5g9kxy7fqd9h8vmmfvdjscqzzsxqyz5vqsp5657mdpk04phuuasjntxwl2t5cgcyv0pa574anx5svdfudf0ueydq9qxpqysgq2cxal5k00nlypltvcl94kkr50vkech6uvnnavqalnl9c8fkc2zpk3ed2j9vwyxg6kg4gcnyms0fafuc3au4f6s3ugaqa8r6d7yq2gggpzqwtnf".to_string();
        let zero_amount_err = is_valid_invoice(payment_request, Some(100), None);
        assert_eq!(
            Ok(()),
            zero_amount_err.await
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
    async fn test_lnurl_validation_with_test_server() {
        init_settings_test();

        // Start test server
        let (lnurl, server_handle) = start_test_server().await;

        // Test basic LNURL validation
        let result = is_valid_invoice(lnurl.clone(), None, None).await;
        assert!(result.is_ok(), "Basic LNURL validation should succeed");

        // Test LNURL validation with amount
        let result = is_valid_invoice(lnurl.clone(), Some(5000), None).await;
        assert!(
            result.is_ok(),
            "LNURL validation with valid amount should succeed"
        );

        // Lightning address validation
        // Test with a valid Lightning address that matches our test server
        let valid_address = "MostroP2P@localhost".to_string();
        let result = is_valid_invoice(valid_address, None, None).await;
        assert!(
            result.is_ok(),
            "Valid Lightning address should pass validation"
        );

        // Test with an invalid Lightning address
        let invalid_address = "nonexistent@localhost".to_string();
        let result = is_valid_invoice(invalid_address, None, None).await;
        assert!(
            result.is_err(),
            "Invalid Lightning address should fail validation"
        );

        // Cleanup
        server_handle.abort();
    }
}
