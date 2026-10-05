//! Which mints a Cashu node accepts for new orders (issue #1046).
//!
//! The maker chooses the mint in `new-order` (`SmallOrder.cashu_mint_url`);
//! the taker accepts it by taking the order. The node only decides whether to
//! publish the order on that mint, from `[cashu].mint_urls`:
//!
//! - a non-empty list is an allow-list: the mint must be in it;
//! - an empty list is an open node: any mint is accepted, except one whose
//!   host is (or resolves to) a non-public address — mostrod sends HTTP
//!   requests to the mint, so an open node must not be steerable into the
//!   operator's network (SSRF). The policy is the LNURL one
//!   ([`crate::lnurl::ip_is_forbidden`]).
//!
//! Mint URLs are compared in the canonical form [`normalize_mint_url`]
//! returns, so `https://Mint.example.com:443/` and `https://mint.example.com`
//! are the same mint.

use std::net::IpAddr;
use std::time::Duration;

use mostro_core::error::CantDoReason;
use reqwest::Url;

/// Longest mint URL accepted, in bytes. Bounds what a maker can make the node
/// store, publish and fetch.
pub const MAX_MINT_URL_LEN: usize = 512;

/// Cap on resolving an open-mode mint's host, so a slow or malicious name
/// server cannot stall the serial message loop.
const MINT_DNS_TIMEOUT: Duration = Duration::from_secs(2);

/// Parse `raw` as a mint URL and return its canonical form: `http`/`https`,
/// a host, no credentials, query or fragment; lowercased host, default port
/// dropped, no trailing `/`.
///
/// The error is a human-readable reason, used verbatim in config errors.
pub fn normalize_mint_url(raw: &str) -> Result<String, String> {
    if raw.len() > MAX_MINT_URL_LEN {
        return Err(format!("is longer than {MAX_MINT_URL_LEN} bytes"));
    }
    let url = Url::parse(raw).map_err(|e| format!("is not a valid URL: {e}"))?;
    if !crate::util::is_http_or_https(&url) {
        return Err(format!(
            "must use http or https, got scheme {:?}",
            url.scheme()
        ));
    }
    if url.host().is_none() {
        return Err("has no host".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("must not carry credentials".to_string());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("must not carry a query or fragment".to_string());
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// The mint a new order escrows on, or why the node refuses to publish it.
///
/// - `requested` set: it must be a valid mint URL, and be in `allowed` when
///   `allowed` is non-empty. With `allowed` empty its host must be public.
/// - `requested` absent: defaults to the only allowed mint when exactly one
///   is configured, so clients that predate the field keep working against
///   single-mint nodes. Otherwise the maker must choose.
///
/// Returns the canonical URL ([`normalize_mint_url`]), which is what the
/// order stores and publishes.
pub async fn resolve_order_mint(
    requested: Option<&str>,
    allowed: &[String],
) -> Result<String, CantDoReason> {
    let Some(raw) = requested else {
        return match allowed {
            [only] => normalize_mint_url(only).map_err(|_| CantDoReason::InvalidMintUrl),
            _ => Err(CantDoReason::InvalidMintUrl),
        };
    };
    let mint = normalize_mint_url(raw).map_err(|_| CantDoReason::InvalidMintUrl)?;
    if allowed.is_empty() {
        ensure_public_mint_host(&mint).await?;
        return Ok(mint);
    }
    let is_allowed = allowed
        .iter()
        .any(|entry| normalize_mint_url(entry).is_ok_and(|entry| entry == mint));
    if is_allowed {
        Ok(mint)
    } else {
        Err(CantDoReason::InvalidMintUrl)
    }
}

/// Reject a mint whose host is, or resolves to, a non-public address.
///
/// An unresolvable host is `InvalidMintUrl`; a resolver that times out is
/// `CashuMintUnavailable`, which the maker can retry.
///
/// The check runs before mostrod first talks to the mint, but the HTTP client
/// resolves the name again, so a name server that answers differently the
/// second time (DNS rebinding) is not covered. Operators who need that
/// guarantee list their mints in `mint_urls`. Redirects are not a way around
/// the check: cdk's HTTP transport does not follow them.
pub async fn ensure_public_mint_host(mint_url: &str) -> Result<(), CantDoReason> {
    let url = Url::parse(mint_url).map_err(|_| CantDoReason::InvalidMintUrl)?;
    let port = url
        .port_or_known_default()
        .ok_or(CantDoReason::InvalidMintUrl)?;
    let host = url.host_str().ok_or(CantDoReason::InvalidMintUrl)?;
    // `host_str` keeps the brackets of an IPv6 literal (`[::1]`).
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    let ips: Vec<IpAddr> = match literal.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => tokio::time::timeout(MINT_DNS_TIMEOUT, tokio::net::lookup_host((host, port)))
            .await
            .map_err(|_| CantDoReason::CashuMintUnavailable)?
            .map_err(|_| CantDoReason::InvalidMintUrl)?
            .map(|addr| addr.ip())
            .collect(),
    };
    if ips.is_empty() || ips.into_iter().any(crate::lnurl::ip_is_forbidden) {
        return Err(CantDoReason::InvalidMintUrl);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lnurl::{allow_private_lnurl_hosts_for_test, AllowPrivateLnurlHostsGuard};

    const MINT_A: &str = "https://mint-a.example.com";
    const MINT_B: &str = "https://mint-b.example.com";

    fn allowed(mints: &[&str]) -> Vec<String> {
        mints.iter().map(|m| m.to_string()).collect()
    }

    #[test]
    fn normalize_mint_url_canonicalises_equivalent_spellings() {
        for raw in [
            "https://mint-a.example.com",
            "https://mint-a.example.com/",
            "HTTPS://Mint-A.Example.com:443/",
        ] {
            assert_eq!(normalize_mint_url(raw).as_deref(), Ok(MINT_A), "{raw}");
        }
        assert_eq!(
            normalize_mint_url("http://127.0.0.1:3338/").as_deref(),
            Ok("http://127.0.0.1:3338")
        );
        assert_eq!(
            normalize_mint_url("https://example.com/mint/").as_deref(),
            Ok("https://example.com/mint")
        );
    }

    #[test]
    fn normalize_mint_url_rejects_unusable_urls() {
        let too_long = format!("https://{}.com", "a".repeat(MAX_MINT_URL_LEN));
        for raw in [
            "",
            "not a url",
            "ftp://mint.example.com",
            "file:///etc/passwd",
            "https://user:pass@mint.example.com",
            "https://mint.example.com/?a=1",
            "https://mint.example.com/#frag",
            too_long.as_str(),
        ] {
            assert!(normalize_mint_url(raw).is_err(), "{raw:?} must be rejected");
        }
    }

    #[tokio::test]
    async fn resolve_order_mint_accepts_a_listed_mint_in_canonical_form() {
        let mint = resolve_order_mint(
            Some("https://MINT-B.example.com/"),
            &allowed(&[MINT_A, MINT_B]),
        )
        .await;
        assert_eq!(mint.as_deref(), Ok(MINT_B));
    }

    #[tokio::test]
    async fn resolve_order_mint_rejects_an_unlisted_mint() {
        let mint = resolve_order_mint(Some(MINT_B), &allowed(&[MINT_A])).await;
        assert_eq!(mint, Err(CantDoReason::InvalidMintUrl));
    }

    #[tokio::test]
    async fn resolve_order_mint_rejects_a_malformed_mint() {
        let mint = resolve_order_mint(Some("ftp://mint-a.example.com"), &allowed(&[])).await;
        assert_eq!(mint, Err(CantDoReason::InvalidMintUrl));
    }

    #[tokio::test]
    async fn resolve_order_mint_defaults_to_the_only_configured_mint() {
        let mint = resolve_order_mint(None, &allowed(&["https://mint-a.example.com/"])).await;
        assert_eq!(mint.as_deref(), Ok(MINT_A));
    }

    #[tokio::test]
    async fn resolve_order_mint_requires_a_choice_with_several_or_no_mints() {
        for list in [allowed(&[MINT_A, MINT_B]), allowed(&[])] {
            let mint = resolve_order_mint(None, &list).await;
            assert_eq!(mint, Err(CantDoReason::InvalidMintUrl), "{list:?}");
        }
    }

    #[tokio::test]
    async fn resolve_order_mint_open_node_accepts_a_public_ip_mint() {
        let mint = resolve_order_mint(Some("https://8.8.8.8/"), &allowed(&[])).await;
        assert_eq!(mint.as_deref(), Ok("https://8.8.8.8"));
    }

    #[tokio::test]
    async fn resolve_order_mint_open_node_rejects_non_public_hosts() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy().await;
        allow_private_lnurl_hosts_for_test(false);
        for raw in [
            "http://127.0.0.1:3338",
            "http://localhost:3338",
            "http://10.0.0.5",
            "http://192.168.1.10:3338",
            "http://169.254.169.254",
            "http://[::1]:3338",
            "http://0.0.0.0",
            // Other spellings of loopback; `Url::parse` normalises them.
            "http://2130706433",
            "http://0x7f.1",
            "http://[::ffff:127.0.0.1]",
        ] {
            let mint = resolve_order_mint(Some(raw), &allowed(&[])).await;
            assert_eq!(mint, Err(CantDoReason::InvalidMintUrl), "{raw}");
        }
    }

    #[tokio::test]
    async fn resolve_order_mint_allow_list_may_name_a_local_mint() {
        // An operator can list a local dev mint explicitly; the host check
        // only guards open nodes.
        let local = "http://127.0.0.1:3338";
        let mint = resolve_order_mint(Some(local), &allowed(&[local])).await;
        assert_eq!(mint.as_deref(), Ok(local));
    }
}
