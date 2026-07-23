use lnurl::lnurl::LnUrl;
use mostro_core::prelude::*;
use once_cell::sync::Lazy;
use reqwest::Client;
use serde_json::Value;
use tracing::{error, warn};

pub static HTTP_CLIENT: Lazy<Client> = Lazy::new(|| {
    Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(concat!("mostro/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("valid reqwest Client")
});
/// Extracts the LNURL from a given address.
/// The address can be in the form of a Lightning Address (user@domain.com format)
/// or a LNURL (lnurl1... format).
/// If the address is a Lightning Address, it is resolved to the corresponding LNURL.
/// If the address is already a LNURL, it is returned as is.
/// # Arguments
/// * `address` - The address to extract the LNURL from
/// # Returns
/// * `Ok(String)` - The extracted LNURL
/// * `Err(MostroError)` - If the address is invalid or cannot be resolved
///
/// Validates the scheme is `http`/`https` before returning: a bech32-decoded
/// LNURL is attacker-controlled input, and every caller of this function
/// (`ln_exists`, `resolv_ln_address`) does an unguarded GET against the
/// result, so this is the one place that check has to hold for all of them.
async fn extract_lnurl(address: &str) -> Result<String, MostroError> {
    let url = if address.to_lowercase().starts_with("lnurl") {
        let lnurl = LnUrl::decode(address.to_string())
            .map_err(|_| MostroInternalErr(ServiceError::LnAddressParseError))?;
        lnurl.url
    } else {
        // Handle Lightning address format
        let (user, domain) = match address.split_once('@') {
            Some((user, domain)) => (user, domain),
            None => return Err(MostroInternalErr(ServiceError::LnAddressParseError)),
        };
        let base_url = if cfg!(test) {
            format!("http://{domain}:8080")
        } else {
            format!("https://{domain}")
        };
        format!("{base_url}/.well-known/lnurlp/{user}")
    };
    let parsed = reqwest::Url::parse(&url)
        .map_err(|_| MostroInternalErr(ServiceError::LnAddressParseError))?;
    if !crate::util::is_http_or_https(&parsed) {
        return Err(MostroInternalErr(ServiceError::LnAddressParseError));
    }
    Ok(url)
}

pub async fn ln_exists(address: &str) -> Result<(), MostroError> {
    // Get the url from the str - could be a LNURL or a Lightning Address
    let url = extract_lnurl(address).await?;
    // Make the request to the LNURL
    let res = HTTP_CLIENT
        .get(url)
        .send()
        .await
        .map_err(|_| MostroInternalErr(ServiceError::NoAPIResponse))?;
    let status = res.status();
    if status.is_success() {
        let body = res
            .text()
            .await
            .map_err(|_| MostroInternalErr(ServiceError::NoAPIResponse))?;
        let body: Value = serde_json::from_str(&body)
            .map_err(|_| MostroInternalErr(ServiceError::MalformedAPIRes))?;
        let tag = body["tag"].as_str().unwrap_or("");
        if tag == "payRequest" {
            return Ok(());
        }
        Err(MostroInternalErr(ServiceError::LnAddressParseError))
    } else {
        Err(MostroInternalErr(ServiceError::LnAddressParseError))
    }
}

/// LUD-12: returns the comment when it fits within `max_len` chars, or
/// `None` if there's no comment to send, the server advertises no support
/// for one (`max_len == 0`), or the comment would have to be truncated — a
/// half-written order id or node pubkey is a worse trace than no trace.
fn fit_comment(comment: Option<&str>, max_len: usize) -> Option<String> {
    let comment = comment.filter(|c| max_len > 0 && c.chars().count() <= max_len)?;
    Some(comment.to_string())
}

/// Builds the LNURL-pay callback URL, adding `amount` (and `comment`, per
/// LUD-12, when the server allows it) as proper query parameters via
/// `query_pairs_mut` — never by string-concatenating onto `callback`, which
/// silently mangles the query if `callback` already carries one (a real LNURL
/// server behavior, e.g. `https://host/cb?id=abc`). Any pre-existing
/// `amount`/`comment` pair on `callback` is dropped first so the values we
/// compute here are the ones actually sent, not appended duplicates.
///
/// Only `http`/`https` callbacks are accepted (same check as `extract_lnurl`
/// applies to the initial address): `callback` comes from a remote LNURL
/// server's response and, for dev-fee payments, can be reached via a
/// buyer-supplied lightning address. This blocks scheme confusion
/// (`javascript:`, `file:`, ...) but not host-based SSRF — a malicious
/// server can still return an `http(s)` callback pointed at a private or
/// link-local address; that's a known gap, tracked separately.
fn build_callback_url(
    callback: &str,
    amount_msat: u64,
    comment: Option<&str>,
    comment_allowed: usize,
) -> Result<reqwest::Url, MostroError> {
    let mut url = reqwest::Url::parse(callback)
        .map_err(|_| MostroInternalErr(ServiceError::LnAddressParseError))?;
    if !crate::util::is_http_or_https(&url) {
        return Err(MostroInternalErr(ServiceError::LnAddressParseError));
    }
    let kept_pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "amount" && k != "comment")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(&kept_pairs)
        .append_pair("amount", &amount_msat.to_string());
    if let Some(value) = fit_comment(comment, comment_allowed) {
        url.query_pairs_mut().append_pair("comment", &value);
    } else if let Some(c) = comment.filter(|_| comment_allowed > 0) {
        warn!(
            "LUD-12 comment dropped: {} chars exceeds server limit of {comment_allowed}; dev-fee trace will be incomplete",
            c.chars().count()
        );
    }
    Ok(url)
}

/// Resolve a Lightning Address or LNURL-pay string into a BOLT11 invoice
/// for `amount` sats.
///
/// `comment` is attached per LUD-12 when the server advertises support for
/// it (`commentAllowed > 0`); otherwise it's silently dropped, matching the
/// pre-LUD-12 behavior.
/// # Arguments
/// * `address` - A Lightning Address or LNURL to resolve
/// * `amount` - Payment amount in satoshis (converted to msat internally)
/// * `comment` - Optional LUD-12 comment, sent only if the server allows it
/// # Returns
/// * `Ok(String)` - The resolved bolt11 invoice, or an empty string if the
///   server rejected the request or doesn't support `payRequest`
/// * `Err(MostroError)` - If the address/LNURL can't be resolved or the HTTP
///   exchange fails
pub async fn resolv_ln_address(
    address: &str,
    amount: u64,
    comment: Option<&str>,
) -> Result<String, MostroError> {
    // Get the url from the str - could be a LNURL or a Lightning Address
    let url = extract_lnurl(address).await?;
    // Convert the amount to msat
    let amount_msat = amount * 1000;

    // Make the request to the LNURL
    let res = HTTP_CLIENT
        .get(url)
        .send()
        .await
        .map_err(|_| MostroInternalErr(ServiceError::MalformedAPIRes))?;
    let status = res.status();
    if status.is_success() {
        let body = res
            .text()
            .await
            .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
        let body: Value = serde_json::from_str(&body)
            .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
        if body["status"].as_str() == Some("ERROR") {
            let reason_len = body["reason"].as_str().map(str::len).unwrap_or(0);
            error!("LNURL address rejected by server (reason length: {reason_len} bytes)");
            return Ok("".to_string());
        }
        let tag = body["tag"].as_str().unwrap_or("");
        if tag != "payRequest" {
            return Ok("".to_string());
        }
        let min = body["minSendable"].as_u64().unwrap_or(0);
        let max = body["maxSendable"].as_u64().unwrap_or(0);
        if min > amount_msat || max < amount_msat {
            return Ok("".to_string());
        }
        let callback = body["callback"].as_str().unwrap_or("");
        let comment_allowed = body["commentAllowed"].as_u64().unwrap_or(0) as usize;
        let callback =
            build_callback_url(callback, amount_msat, comment, comment_allowed)?.to_string();
        let res = HTTP_CLIENT
            .get(callback)
            .send()
            .await
            .map_err(|_| MostroInternalErr(ServiceError::MalformedAPIRes))?;
        let status = res.status();
        if status.is_success() {
            let body = res
                .text()
                .await
                .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
            let body: Value = serde_json::from_str(&body)
                .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
            if body["status"].as_str() == Some("ERROR") {
                let reason_len = body["reason"].as_str().map(str::len).unwrap_or(0);
                error!("LNURL callback rejected by server (reason length: {reason_len} bytes)");
                return Ok("".to_string());
            }
            let pr = body["pr"].as_str().unwrap_or("");

            return Ok(pr.to_string());
        }
        Ok("".to_string())
    } else {
        Ok("".to_string())
    }
}

#[cfg(test)]
mod tests {
    //! Parsing/validation coverage only: the HTTP round-trips of
    //! `ln_exists` / `resolv_ln_address` need a live LNURL endpoint and are
    //! exercised by the integration-style server tests in
    //! `lightning::invoice`.
    use super::*;

    #[tokio::test]
    async fn extract_lnurl_decodes_bech32_lnurl() {
        let url = "https://example.com/.well-known/lnurlp/alice";
        let encoded = LnUrl {
            url: url.to_string(),
        }
        .encode();
        assert!(encoded.to_lowercase().starts_with("lnurl1"));

        let extracted = extract_lnurl(&encoded).await.expect("valid LNURL decodes");
        assert_eq!(extracted, url);
    }

    #[tokio::test]
    async fn extract_lnurl_rejects_malformed_bech32() {
        assert!(extract_lnurl("lnurl1notvalidbech32").await.is_err());
    }

    #[tokio::test]
    async fn extract_lnurl_rejects_non_http_scheme() {
        // A bech32-decoded LNURL is attacker-controlled: it must not be
        // able to smuggle a javascript:/file:/ftp: URL past extract_lnurl,
        // since every caller does an unguarded GET on the result.
        let encoded = LnUrl {
            url: "javascript:alert(1)".to_string(),
        }
        .encode();
        assert!(extract_lnurl(&encoded).await.is_err());
    }

    #[tokio::test]
    async fn extract_lnurl_builds_wellknown_url_for_lightning_address() {
        // cfg!(test) pins lightning addresses to the local test host form.
        let extracted = extract_lnurl("alice@127.0.0.1")
            .await
            .expect("lightning address parses");
        assert_eq!(extracted, "http://127.0.0.1:8080/.well-known/lnurlp/alice");
    }

    #[tokio::test]
    async fn extract_lnurl_rejects_address_without_at() {
        assert!(extract_lnurl("not-a-lightning-address").await.is_err());
    }

    #[tokio::test]
    async fn ln_exists_propagates_parse_error_before_any_request() {
        assert!(ln_exists("no-at-sign-here").await.is_err());
    }

    #[tokio::test]
    async fn resolv_ln_address_propagates_parse_error_before_any_request() {
        assert!(resolv_ln_address("no-at-sign-here", 1_000, None)
            .await
            .is_err());
    }

    #[test]
    fn build_callback_url_adds_amount_as_its_own_param() {
        let url = build_callback_url("https://pay.example.com/cb", 100_000, None, 0).unwrap();
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![("amount".into(), "100000".into())]
        );
    }

    #[test]
    fn build_callback_url_preserves_existing_query_params() {
        // Regression test: callback already carries its own query string
        // (common in the wild, e.g. LNbits-style `?id=...`). The old
        // `format!("{callback}?amount={amount_msat}")` approach produced a
        // second `?`, which is not a delimiter, so `amount` got swallowed
        // into the `id` value instead of becoming its own parameter.
        let url =
            build_callback_url("https://pay.example.com/cb?id=abc", 100_000, None, 0).unwrap();
        let pairs = url.query_pairs().collect::<Vec<_>>();
        assert_eq!(
            pairs,
            vec![
                ("id".into(), "abc".into()),
                ("amount".into(), "100000".into())
            ]
        );
    }

    #[test]
    fn build_callback_url_adds_comment_when_allowed() {
        let url =
            build_callback_url("https://pay.example.com/cb", 100_000, Some("order=1"), 50).unwrap();
        let pairs = url.query_pairs().collect::<Vec<_>>();
        assert_eq!(pairs[0], ("amount".into(), "100000".into()));
        assert_eq!(pairs[1], ("comment".into(), "order=1".into()));
    }

    #[test]
    fn build_callback_url_omits_comment_when_not_allowed() {
        let url =
            build_callback_url("https://pay.example.com/cb", 100_000, Some("order=1"), 0).unwrap();
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![("amount".into(), "100000".into())]
        );
    }

    #[test]
    fn fit_comment_none_when_not_allowed() {
        assert_eq!(fit_comment(Some("order=1"), 0), None);
    }

    #[test]
    fn fit_comment_none_when_no_comment() {
        assert_eq!(fit_comment(None, 50), None);
    }

    #[test]
    fn fit_comment_passes_through_when_short_enough() {
        assert_eq!(
            fit_comment(Some("order=1 node=abc"), 50),
            Some("order=1 node=abc".to_string())
        );
    }

    #[test]
    fn fit_comment_none_when_too_long_for_server_limit() {
        assert_eq!(fit_comment(Some("order=1 node=abc"), 7), None);
    }

    #[test]
    fn build_callback_url_rejects_non_http_scheme() {
        assert!(build_callback_url("javascript:alert(1)", 100_000, None, 0).is_err());
        assert!(build_callback_url("ftp://host/cb", 100_000, None, 0).is_err());
    }

    #[test]
    fn build_callback_url_drops_preexisting_amount_and_comment() {
        let url = build_callback_url(
            "https://pay.example.com/cb?amount=5&comment=old",
            100_000,
            Some("new"),
            10,
        )
        .unwrap();
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![
                ("amount".into(), "100000".into()),
                ("comment".into(), "new".into())
            ]
        );
    }
}
