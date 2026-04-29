//! BIP-353 DNS resolution for `user@domain` payment addresses.
//!
//! Resolves human-readable payment addresses to BOLT12 offers via
//! DNSSEC-validated DNS TXT records. Uses DNS-over-HTTPS (DoH) with
//! a trusted resolver (default: Cloudflare) to get DNSSEC validation
//! without running a local validating resolver.
//!
//! Resolution flow:
//! 1. `user@domain.com` → DNS TXT at `user.user._bitcoin-payment.domain.com`
//! 2. TXT record contains `bitcoin:?lno=lno1...`
//! 3. Extract and return the `lno1...` offer string
//!
//! Falls back gracefully on any failure — the caller should try LNURL
//! next.

use crate::config::settings::Settings;
use crate::lnurl::HTTP_CLIENT;
use mostro_core::prelude::*;
use serde::Deserialize;

/// Resolves a BIP-353 `user@domain` address to a BOLT12 offer string.
///
/// Returns `Ok(Some(offer))` if resolution succeeds with a valid
/// DNSSEC-authenticated offer. Returns `Ok(None)` on any failure so
/// the caller can fall through to LNURL. Only returns `Err` for
/// truly malformed input.
pub async fn resolve_bip353(address: &str) -> Result<Option<String>, MostroError> {
    let ln = Settings::get_ln();

    if !ln.bip353_enabled || !ln.lndk_enabled {
        return Ok(None);
    }

    let (user, domain) = match address.split_once('@') {
        Some(pair) => pair,
        None => return Ok(None),
    };

    if user.is_empty() || domain.is_empty() {
        return Ok(None);
    }

    let dns_name = build_dns_name(user, domain);
    let resolver_url = &ln.bip353_doh_resolver;

    let response = match query_doh(resolver_url, &dns_name).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!("BIP-353 DoH query failed for {address}: {e}");
            return Ok(None);
        }
    };

    // DNS status 0 = NOERROR
    if response.status != 0 {
        tracing::debug!(
            "BIP-353 DNS returned status {} for {address}",
            response.status
        );
        return Ok(None);
    }

    // DNSSEC validation via the AD (Authenticated Data) flag
    if !response.ad && !ln.bip353_skip_dnssec {
        tracing::warn!(
            "BIP-353 DNSSEC not validated for {address} (AD=false). \
             Set bip353_skip_dnssec=true to override (regtest only)."
        );
        return Ok(None);
    }

    // Extract lno offer from TXT records
    for answer in &response.answer {
        if let Some(offer) = parse_bitcoin_uri(&answer.data) {
            if offer.starts_with("lno1") {
                tracing::info!("BIP-353 resolved {address} → bolt12 offer");
                return Ok(Some(offer));
            }
        }
    }

    tracing::debug!("BIP-353 no lno offer found in TXT records for {address}");
    Ok(None)
}

/// Constructs the BIP-353 DNS name for a given user and domain.
fn build_dns_name(user: &str, domain: &str) -> String {
    format!("{user}.user._bitcoin-payment.{domain}")
}

/// Parses a `bitcoin:` URI and extracts the `lno` query parameter.
///
/// DNS TXT records may be split across multiple quoted chunks:
/// `"bitcoin:?lno=lno1abc" "def..."` — these are concatenated first.
fn parse_bitcoin_uri(txt: &str) -> Option<String> {
    // Concatenate quoted TXT chunks: `"part1" "part2"` → `part1part2`
    let concatenated: String = txt
        .split('"')
        .enumerate()
        .filter_map(|(i, s)| if i % 2 == 1 { Some(s) } else { None })
        .collect::<String>();

    // If no quotes found, use the raw string
    let txt = if concatenated.is_empty() {
        txt.trim().to_string()
    } else {
        concatenated
    };

    let query = txt.split_once('?').map(|(_, rest)| rest)?;

    for pair in query.split('&') {
        if let Some(("lno", value)) = pair.split_once('=') {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }

    None
}

// ── DoH (DNS-over-HTTPS) client ────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DohResponse {
    #[serde(rename = "Status")]
    status: u32,
    #[serde(rename = "AD", default)]
    ad: bool,
    #[serde(rename = "Answer", default)]
    answer: Vec<DohAnswer>,
}

#[derive(Debug, Deserialize)]
struct DohAnswer {
    data: String,
}

/// Queries a DNS-over-HTTPS resolver for TXT records.
async fn query_doh(resolver_url: &str, name: &str) -> Result<DohResponse, MostroError> {
    let resp = HTTP_CLIENT
        .get(resolver_url)
        .query(&[("name", name), ("type", "TXT")])
        .header("Accept", "application/dns-json")
        .send()
        .await
        .map_err(|e| {
            MostroInternalErr(ServiceError::LnPaymentError(format!("DoH request: {e}")))
        })?;

    if !resp.status().is_success() {
        return Err(MostroInternalErr(ServiceError::LnPaymentError(format!(
            "DoH returned HTTP {}",
            resp.status()
        ))));
    }

    resp.json::<DohResponse>().await.map_err(|e| {
        MostroInternalErr(ServiceError::LnPaymentError(format!(
            "DoH response parse: {e}"
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_name_construction() {
        assert_eq!(
            build_dns_name("alice", "example.com"),
            "alice.user._bitcoin-payment.example.com"
        );
        assert_eq!(
            build_dns_name("bob", "walletofsatoshi.com"),
            "bob.user._bitcoin-payment.walletofsatoshi.com"
        );
    }

    #[test]
    fn parse_simple_lno() {
        let txt = "bitcoin:?lno=lno1pgqfoo";
        assert_eq!(parse_bitcoin_uri(txt), Some("lno1pgqfoo".to_string()));
    }

    #[test]
    fn parse_lno_with_other_params() {
        let txt = "bitcoin:?lno=lno1abc&sp=sp1def";
        assert_eq!(parse_bitcoin_uri(txt), Some("lno1abc".to_string()));
    }

    #[test]
    fn parse_quoted_txt_record() {
        let txt = "\"bitcoin:?lno=lno1xyz\"";
        assert_eq!(parse_bitcoin_uri(txt), Some("lno1xyz".to_string()));
    }

    #[test]
    fn parse_chunked_txt_record() {
        // Real-world format: DNS TXT records split across multiple quoted chunks
        let txt = "\"bitcoin:?lno=lno1abc\" \"def\"";
        assert_eq!(parse_bitcoin_uri(txt), Some("lno1abcdef".to_string()));
    }

    #[test]
    fn parse_no_lno_param() {
        let txt = "bitcoin:?sp=sp1abc";
        assert_eq!(parse_bitcoin_uri(txt), None);
    }

    #[test]
    fn parse_no_query_string() {
        let txt = "bitcoin:bc1qfoo";
        assert_eq!(parse_bitcoin_uri(txt), None);
    }

    #[test]
    fn parse_empty() {
        assert_eq!(parse_bitcoin_uri(""), None);
        assert_eq!(parse_bitcoin_uri("\"\""), None);
    }

    #[test]
    fn doh_response_deserialize() {
        let json = r#"{
            "Status": 0,
            "AD": true,
            "Answer": [
                {"name": "test", "type": 16, "TTL": 300, "data": "\"bitcoin:?lno=lno1test\""}
            ]
        }"#;
        let resp: DohResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.status, 0);
        assert!(resp.ad);
        assert_eq!(resp.answer.len(), 1);
        assert_eq!(
            parse_bitcoin_uri(&resp.answer[0].data),
            Some("lno1test".to_string())
        );
    }

    #[test]
    fn doh_response_no_ad_flag() {
        let json = r#"{"Status": 0, "Answer": []}"#;
        let resp: DohResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.ad);
    }
}
