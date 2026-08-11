use crate::config::settings::Settings;
use crate::lnurl::ln_exists;
use crate::LN_STATUS;

use chrono::prelude::*;
use chrono::TimeDelta;
use lightning_invoice::{Bolt11Invoice, Currency, SignedRawBolt11Invoice};
use lnurl::lightning_address::LightningAddress;
use lnurl::lnurl::LnUrl;
use mostro_core::prelude::*;
use std::str::FromStr;

/// Maps a network name as reported by LND's `GetInfo` to the BOLT11 currency
/// a node on that chain is able to pay.
fn currency_for_network(network: &str) -> Option<Currency> {
    match network {
        "mainnet" => Some(Currency::Bitcoin),
        "testnet" => Some(Currency::BitcoinTestnet),
        "regtest" => Some(Currency::Regtest),
        "signet" => Some(Currency::Signet),
        "simnet" => Some(Currency::Simnet),
        _ => None,
    }
}

/// The currency payout invoices must carry, or `None` when the node's chain is
/// unknown. `LN_STATUS` is populated by the startup `GetInfo` probe, so it is
/// always set while the daemon serves requests; the `None` case covers tests
/// and any network name we don't map. Callers skip the check rather than
/// reject, so an unmapped chain never blocks trading.
fn node_currency() -> Option<Currency> {
    LN_STATUS
        .get()?
        .networks
        .first()
        .and_then(|network| currency_for_network(network))
}

/// Whether a node on `expected_currency` is able to pay `invoice`. `None` means
/// the node's chain is unknown, in which case the check is skipped.
fn invoice_currency_is_payable(
    invoice: &Bolt11Invoice,
    expected_currency: Option<Currency>,
) -> bool {
    match expected_currency {
        Some(expected_currency) => invoice.currency() == expected_currency,
        None => true,
    }
}

/// Checks the properties that must hold for any invoice this node is about to
/// pay, whatever its origin: it has to be on the node's own chain, its final
/// CLTV delta has to stay within the configured bound, and it must not have
/// expired already.
///
/// Kept apart from [`validate_bolt11_invoice`] because the amount and expiry
/// rules there do not apply to invoices minted by an LNURL server: those are
/// resolved at payment time and are routinely short-lived, so the
/// `invoice_expiration_window` rule would reject legitimate payouts.
pub fn validate_payout_invoice(invoice: &Bolt11Invoice) -> Result<(), MostroError> {
    // LND cannot pay another chain's invoice. Rejecting here keeps the failure
    // at validation time instead of mid-payout.
    if !invoice_currency_is_payable(invoice, node_currency()) {
        return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
    }

    // Bound the payee-chosen final CLTV delta: a large value lets the payee
    // hold the outgoing HTLC, and the routing liquidity behind it, for weeks.
    if invoice.min_final_cltv_expiry_delta() > Settings::get_ln().max_final_cltv_expiry_delta as u64
    {
        return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
    }

    // An already-expired invoice cannot be paid. This is not the
    // `invoice_expiration_window` rule, which demands remaining lifetime an
    // LNURL-minted invoice has no reason to satisfy.
    if invoice.is_expired() {
        return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
    }

    Ok(())
}

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
/// - Chain match against the node's own network
/// - Final CLTV expiry delta bound
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
/// - Invoice currency must match the chain the node runs on, when known
/// - `min_final_cltv_expiry_delta` must not exceed `max_final_cltv_expiry_delta`
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

    // Chain and final-CLTV rules, shared with the invoices an LNURL server
    // hands us at payment time.
    validate_payout_invoice(&invoice)?;

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

    use mostro_core::error::{MostroError::MostroInternalErr, ServiceError};
    use serde_json::json;
    use tokio::net::TcpListener;
    use toml;

    fn init_settings_test() {
        let config_tpl = include_bytes!("../../settings.tpl.toml");
        let config_tpl =
            std::str::from_utf8(config_tpl).expect("Invalid UTF-8 in template config file");
        let mut test_settings: Settings =
            toml::from_str(config_tpl).expect("Failed to parse template config file");
        // The template ships a placeholder nsec; install a parseable one so
        // whichever module wins the MOSTRO_CONFIG race leaves get_keys()
        // usable for every other test.
        test_settings.nostr.nsec_privkey = secrecy::SecretString::from(
            "nsec13as48eum93hkg7plv526r9gjpa0uc52zysqm93pmnkca9e69x6tsdjmdxd",
        );
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

        // Create LnUrl from URL and encode to string
        let lnurl_obj = LnUrl { url: url.clone() };
        let lnurl = lnurl_obj.encode();

        (lnurl, handle)
    }

    /// Seed the chain global so the network check is active, and return the
    /// currency it resolves to. `LN_STATUS` is a process-wide `OnceLock` that
    /// other tests also seed, so the installed value may not be ours — read
    /// back whatever won.
    fn init_ln_status_test() -> Option<Currency> {
        let _ = LN_STATUS.set(crate::lightning::LnStatus {
            version: "test".to_string(),
            node_pubkey: "00".repeat(32),
            commit_hash: "test".to_string(),
            node_alias: "test-node".to_string(),
            chains: vec!["bitcoin".to_string()],
            networks: vec!["mainnet".to_string()],
            uris: vec![],
        });
        // Setting first means the global is initialised from here on, so every
        // later read in this process returns the same value. It may still be
        // `None`: other modules install stubs with no networks, in which case
        // the network check is inactive for the whole run.
        node_currency()
    }

    /// A currency that passes the network check whatever `LN_STATUS` ended up
    /// holding.
    fn payable_currency() -> Currency {
        init_ln_status_test().unwrap_or(Currency::Bitcoin)
    }

    /// A currency the node cannot pay, whichever chain it runs on.
    fn foreign_currency(node: Currency) -> Currency {
        match node {
            Currency::Bitcoin => Currency::BitcoinTestnet,
            _ => Currency::Bitcoin,
        }
    }

    /// Build a freshly-signed BOLT11 invoice locally (no network, no LND).
    /// `amount_msat = None` produces a no-amount invoice.
    fn build_test_invoice(amount_msat: Option<u64>, expiry_secs: u64) -> String {
        build_test_invoice_with(
            amount_msat,
            expiry_secs,
            payable_currency(),
            DEFAULT_TEST_CLTV_DELTA,
        )
    }

    /// Typical wallet value, well under any sane `max_final_cltv_expiry_delta`.
    const DEFAULT_TEST_CLTV_DELTA: u64 = 144;

    /// An invoice on the node's own chain whose lifetime already elapsed:
    /// issued an hour ago with a one-minute expiry.
    fn build_expired_test_invoice() -> String {
        use bitcoin::hashes::{sha256, Hash};
        use bitcoin::secp256k1::{Secp256k1, SecretKey};
        use lightning_invoice::{InvoiceBuilder, PaymentSecret};
        use std::time::{Duration, SystemTime};

        let secp = Secp256k1::new();
        let private_key = SecretKey::from_slice(&[0x42; 32]).expect("valid secret key");
        let payment_hash = sha256::Hash::hash(&[0u8; 32]);
        let issued_at = SystemTime::now() - Duration::from_secs(3_600);

        InvoiceBuilder::new(payable_currency())
            .description("mostro coverage test invoice".into())
            .payment_hash(payment_hash)
            .payment_secret(PaymentSecret([42u8; 32]))
            .timestamp(issued_at)
            .min_final_cltv_expiry_delta(DEFAULT_TEST_CLTV_DELTA)
            .expiry_time(Duration::from_secs(60))
            .amount_milli_satoshis(1_000_000)
            .build_signed(|hash| secp.sign_ecdsa_recoverable(hash, &private_key))
            .expect("valid signed invoice")
            .to_string()
    }

    /// `build_test_invoice` with the two fields the network and final-CLTV
    /// checks read.
    fn build_test_invoice_with(
        amount_msat: Option<u64>,
        expiry_secs: u64,
        currency: Currency,
        min_final_cltv_expiry_delta: u64,
    ) -> String {
        use bitcoin::hashes::{sha256, Hash};
        use bitcoin::secp256k1::{Secp256k1, SecretKey};
        use lightning_invoice::{InvoiceBuilder, PaymentSecret};
        use std::time::Duration;

        let secp = Secp256k1::new();
        let private_key = SecretKey::from_slice(&[0x42; 32]).expect("valid secret key");
        let payment_hash = sha256::Hash::hash(&[0u8; 32]);

        let builder = InvoiceBuilder::new(currency)
            .description("mostro coverage test invoice".into())
            .payment_hash(payment_hash)
            .payment_secret(PaymentSecret([42u8; 32]))
            .current_timestamp()
            .min_final_cltv_expiry_delta(min_final_cltv_expiry_delta)
            .expiry_time(Duration::from_secs(expiry_secs));
        let builder = match amount_msat {
            Some(msat) => builder.amount_milli_satoshis(msat),
            None => builder,
        };
        builder
            .build_signed(|hash| secp.sign_ecdsa_recoverable(hash, &private_key))
            .expect("valid signed invoice")
            .to_string()
    }

    #[test]
    fn currency_for_network_maps_every_lnd_chain() {
        assert_eq!(currency_for_network("mainnet"), Some(Currency::Bitcoin));
        assert_eq!(
            currency_for_network("testnet"),
            Some(Currency::BitcoinTestnet)
        );
        assert_eq!(currency_for_network("regtest"), Some(Currency::Regtest));
        assert_eq!(currency_for_network("signet"), Some(Currency::Signet));
        assert_eq!(currency_for_network("simnet"), Some(Currency::Simnet));
        // An unmapped name skips the check instead of blocking trading.
        assert_eq!(currency_for_network("testnet4"), None);
    }

    /// The reason `validate_payout_invoice` exists apart from
    /// `validate_bolt11_invoice`: LNURL servers mint short-lived invoices, and
    /// folding the expiry-window rule into the payout path would reject them.
    #[tokio::test]
    async fn validate_payout_invoice_ignores_the_expiry_window() {
        init_settings_test();
        let window = Settings::get_ln().invoice_expiration_window as u64;
        let short_lived = build_test_invoice_with(Some(1_000_000), 60, payable_currency(), 144);

        assert!(
            validate_payout_invoice(&decode_invoice(&short_lived).expect("must decode")).is_ok(),
            "a short-lived LNURL invoice must pass the payout checks"
        );

        if window > 60 {
            assert_eq!(
                is_valid_invoice(short_lived, None, None).await,
                Err(MostroInternalErr(ServiceError::InvoiceInvalidError)),
                "the same invoice is rejected by the buyer-supplied path"
            );
        }
    }

    /// Already expired is rejected; merely short-lived is not. The two are
    /// separate rules and only the first belongs in the payout path.
    #[tokio::test]
    async fn validate_payout_invoice_rejects_only_expired_invoices() {
        init_settings_test();

        let expired = decode_invoice(&build_expired_test_invoice()).expect("must decode");
        assert!(expired.is_expired(), "fixture must be expired");
        assert_eq!(
            validate_payout_invoice(&expired),
            Err(MostroInternalErr(ServiceError::InvoiceInvalidError)),
            "an expired invoice can never be paid"
        );

        // 60s of remaining lifetime is below `invoice_expiration_window` but
        // perfectly payable, so the payout path must accept it.
        let short_lived = decode_invoice(&build_test_invoice_with(
            Some(1_000_000),
            60,
            payable_currency(),
            144,
        ))
        .expect("must decode");
        assert!(!short_lived.is_expired());
        assert!(validate_payout_invoice(&short_lived).is_ok());
    }

    #[tokio::test]
    async fn validate_payout_invoice_rejects_excessive_final_cltv_delta() {
        init_settings_test();
        let max_delta = Settings::get_ln().max_final_cltv_expiry_delta as u64;
        let invoice =
            build_test_invoice_with(Some(1_000_000), 86_400, payable_currency(), max_delta + 1);
        assert_eq!(
            validate_payout_invoice(&decode_invoice(&invoice).expect("must decode")),
            Err(MostroInternalErr(ServiceError::InvoiceInvalidError))
        );
    }

    #[tokio::test]
    async fn validate_payout_invoice_rejects_another_chain() {
        init_settings_test();
        // Inactive without a mapped chain; see
        // `invoice_currency_is_payable_only_on_the_node_chain`.
        let Some(node_currency) = init_ln_status_test() else {
            return;
        };
        let invoice = build_test_invoice_with(
            Some(1_000_000),
            86_400,
            foreign_currency(node_currency),
            144,
        );
        assert_eq!(
            validate_payout_invoice(&decode_invoice(&invoice).expect("must decode")),
            Err(MostroInternalErr(ServiceError::InvoiceInvalidError))
        );
    }

    /// The chain comparison itself, independent of the shared `LN_STATUS`.
    #[test]
    fn invoice_currency_is_payable_only_on_the_node_chain() {
        let invoice = decode_invoice(&build_test_invoice_with(
            Some(1_000_000),
            86_400,
            Currency::Bitcoin,
            144,
        ))
        .expect("must decode");

        assert!(invoice_currency_is_payable(
            &invoice,
            Some(Currency::Bitcoin)
        ));
        assert!(!invoice_currency_is_payable(
            &invoice,
            Some(Currency::BitcoinTestnet)
        ));
        assert!(!invoice_currency_is_payable(
            &invoice,
            Some(Currency::Regtest)
        ));
        // Unknown chain: skipped rather than rejected.
        assert!(invoice_currency_is_payable(&invoice, None));
    }

    #[tokio::test]
    async fn test_invoice_for_another_chain_is_rejected() {
        init_settings_test();
        // With no mapped chain installed the check is inactive by design and
        // there is nothing to assert end to end; the comparison itself is
        // covered by `invoice_currency_is_payable_only_on_the_node_chain`.
        let Some(node_currency) = init_ln_status_test() else {
            return;
        };
        let payment_request = build_test_invoice_with(
            Some(1_000_000),
            86_400,
            foreign_currency(node_currency),
            144,
        );
        let result = is_valid_invoice(payment_request, Some(1_000), None).await;
        assert_eq!(
            result,
            Err(MostroInternalErr(ServiceError::InvoiceInvalidError)),
            "an invoice the node cannot pay must be rejected at validation time"
        );
    }

    #[tokio::test]
    async fn test_invoice_for_the_node_chain_is_accepted() {
        init_settings_test();
        let payment_request =
            build_test_invoice_with(Some(1_000_000), 86_400, payable_currency(), 144);
        let result = is_valid_invoice(payment_request, Some(1_000), None).await;
        assert!(
            result.is_ok(),
            "an invoice on the node's own chain must pass: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_excessive_final_cltv_delta_is_rejected() {
        init_settings_test();
        // Read the effective bound so the test holds whichever settings won
        // the process-wide OnceLock race.
        let max_delta = Settings::get_ln().max_final_cltv_expiry_delta as u64;
        let payment_request =
            build_test_invoice_with(Some(1_000_000), 86_400, payable_currency(), max_delta + 1);
        let result = is_valid_invoice(payment_request, Some(1_000), None).await;
        assert_eq!(
            result,
            Err(MostroInternalErr(ServiceError::InvoiceInvalidError)),
            "a final CLTV delta above the configured bound must be rejected"
        );
    }

    #[tokio::test]
    async fn test_final_cltv_delta_at_the_bound_is_accepted() {
        init_settings_test();
        let max_delta = Settings::get_ln().max_final_cltv_expiry_delta as u64;
        let payment_request =
            build_test_invoice_with(Some(1_000_000), 86_400, payable_currency(), max_delta);
        let result = is_valid_invoice(payment_request, Some(1_000), None).await;
        assert!(
            result.is_ok(),
            "the bound itself must be inclusive: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_fresh_invoice_with_matching_amount_is_valid() {
        init_settings_test();
        // 1000 sats invoice; caller expects amount 1100 with 100 sats fee →
        // expected_sats_amount = 1000 = invoice amount → valid.
        let payment_request = build_test_invoice(Some(1_000_000), 86_400);
        let result = is_valid_invoice(payment_request, Some(1_100), Some(100)).await;
        assert!(
            result.is_ok(),
            "fresh matching invoice must pass: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_fresh_invoice_without_amount_check_is_valid() {
        init_settings_test();
        // No expected amount: only min-amount and expiry windows apply.
        let payment_request = build_test_invoice(Some(1_000_000), 86_400);
        let result = is_valid_invoice(payment_request, None, None).await;
        assert!(result.is_ok(), "fresh invoice must pass: {result:?}");
    }

    #[tokio::test]
    async fn test_fresh_invoice_amount_mismatch_is_rejected() {
        init_settings_test();
        // Invoice carries 1000 sats but the caller expects 5000 - 0 fee.
        let payment_request = build_test_invoice(Some(1_000_000), 86_400);
        let result = is_valid_invoice(payment_request, Some(5_000), None).await;
        assert_eq!(
            result,
            Err(MostroInternalErr(ServiceError::InvoiceInvalidError)),
            "amount mismatch on a non-expired invoice must be rejected"
        );
    }

    #[tokio::test]
    async fn test_fee_larger_than_amount_overflows_and_is_rejected() {
        init_settings_test();
        // fee > amount → checked_sub underflows → invalid.
        let payment_request = build_test_invoice(Some(1_000_000), 86_400);
        let result = is_valid_invoice(payment_request, Some(100), Some(200)).await;
        assert_eq!(
            result,
            Err(MostroInternalErr(ServiceError::InvoiceInvalidError)),
            "fee larger than expected amount must be rejected (subtraction underflow)"
        );
    }

    #[tokio::test]
    async fn test_invoice_expiring_before_expiration_window_is_rejected() {
        init_settings_test();
        // Non-expired invoice whose remaining lifetime (60s) is shorter than
        // the configured expiration window. The assertion adapts to whichever
        // global settings won the process-wide OnceLock race.
        let window = Settings::get_ln().invoice_expiration_window;
        let payment_request = build_test_invoice(Some(1_000_000), 60);
        let result = is_valid_invoice(payment_request, None, None).await;
        if window as u64 > 60 {
            assert_eq!(
                result,
                Err(MostroInternalErr(ServiceError::InvoiceInvalidError)),
                "invoice expiring inside the window must be rejected"
            );
        } else {
            assert!(result.is_ok(), "window {window} accepts short invoices");
        }
    }

    #[tokio::test]
    async fn test_zero_amount_fresh_invoice_skips_amount_and_min_checks() {
        init_settings_test();
        // A fresh no-amount invoice: amount_sat == 0 skips both the equality
        // check (explicitly allowed) and the min-payment floor.
        let payment_request = build_test_invoice(None, 86_400);
        let result = is_valid_invoice(payment_request, Some(1_000), None).await;
        assert!(
            result.is_ok(),
            "zero-amount invoice must pass amount checks: {result:?}"
        );
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
        let payment_request = "lnbc01p5dzna7pp5e23a62fcx6mcyhn9cqppln52ge4xpv0p8fv44a5jewtdypvuj7rqcqzyssp5xwcx4hn7sahsaq3y5ln8yt3qwsxqwtzwac0d32s825rcnp4yps5q9q7sqqqqqqqqqqqqqqqqqqqsqqqqqysgqdqqmqz9gxqyjw5qrzjqwryaup9lh50kkranzgcdnn2fgvx390wgj5jd07rwr3vxeje0glclludlw6z8nzdzcqqqqlgqqqqqeqqjqaq8mpxmhte2h3t0pnw7ey6hu5wvzd5ftm236jwf4whnddvwggw8ka343d9ecq93camv7lju889e4etjfc2mguvdcdkfqc00alc4lfusq7x0jsx".to_string();
        // Check zero amount
        let invoice = decode_invoice(&payment_request).expect("failed to decode invoice");
        assert_eq!(invoice.amount_milli_satoshis().unwrap(), 0);
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
