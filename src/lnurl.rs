//! LNURL-pay and Lightning Address resolution for Mostro.
//!
//! Every outbound LNURL HTTP fetch goes through [`lnurl_get`], which:
//! - allows only `https`, plus plain `http` for `.onion` hosts as LUD-01
//!   permits (see [`lnurl_scheme_allowed`]), and rejects userinfo
//! - resolves DNS under a short timeout and rejects forbidden destinations
//!   (loopback/private unless the test-only allow flag is set, plus link-local,
//!   CGNAT `100.64.0.0/10`, NAT64 `64:ff9b::/96`, multicast, etc.)
//! - pins the request to a checked [`SocketAddr`] (no DNS rebinding) and
//!   disables redirects and system proxies (a proxy would resolve the host
//!   itself and bypass the pin)
//! - bounds connect and request duration so a hanging host cannot stall the
//!   serial message loop for long
//!
//! Callers (`ln_exists`, `resolv_ln_address`) map LNURL-level `ERROR` / missing
//! `payRequest` / empty `pr` to `Err` rather than soft empty success.

use lnurl::lnurl::LnUrl;
use mostro_core::prelude::*;
use reqwest::redirect::Policy;
use reqwest::{Client, Url};
use serde_json::Value;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tracing::{error, warn};

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

/// Cap on a single LNURL HTTP round-trip (connect + response).
/// Kept short so a silent/hanging host cannot stall the serial message
/// loop for long (DoS bound, not elimination).
const LNURL_REQUEST_TIMEOUT: Duration = Duration::from_secs(4);

/// Cap on establishing the TCP/TLS connection alone, so an unroutable or
/// filtered host fails before burning the full request budget.
const LNURL_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Cap on DNS resolution before the HTTP client starts. Without this,
/// a hanging resolver (slow/malicious NS) can stall the serial message
/// loop longer than the HTTP timeouts below.
const LNURL_DNS_TIMEOUT: Duration = Duration::from_secs(2);

/// Cap on a single LNURL response body, in bytes. The timeouts above bound
/// how long a buyer-chosen server may talk, not how much it may send: 4 s at
/// datacenter speed is hundreds of MiB held in memory. Measured legitimate
/// bodies are 0.3-0.8 KB, and 4.6 KB for the one server seen shipping a base64
/// thumbnail in `metadata` (LUD-06 allows up to ~133 KiB, which this cap would
/// refuse). Mostro never reads `metadata`, so such a server is not supported.
const MAX_LNURL_RESPONSE_BYTES: usize = 64 * 1024;

#[cfg(test)]
static ALLOW_PRIVATE_LNURL_HOSTS: AtomicBool = AtomicBool::new(false);

/// Serializes tests that mutate [`ALLOW_PRIVATE_LNURL_HOSTS`] so parallel
/// tokio tests cannot flip the flag under each other.
#[cfg(test)]
static LNURL_HOST_POLICY_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Test-only escape hatch so local mock LNURL servers on loopback / RFC1918
/// addresses keep working. Link-local (e.g. cloud metadata `169.254.0.0/16`)
/// stays forbidden even when this is set. Production builds never set this.
#[cfg(test)]
pub fn allow_private_lnurl_hosts_for_test(allow: bool) {
    ALLOW_PRIVATE_LNURL_HOSTS.store(allow, Ordering::SeqCst);
}

#[cfg(test)]
fn private_hosts_allowed() -> bool {
    ALLOW_PRIVATE_LNURL_HOSTS.load(Ordering::SeqCst)
}

#[cfg(not(test))]
fn private_hosts_allowed() -> bool {
    false
}

/// RAII guard that enables loopback/RFC1918 LNURL hosts for the duration of
/// a local mock-server test, then restores the previous flag.
#[cfg(test)]
pub struct AllowPrivateLnurlHostsGuard {
    previous: bool,
}

#[cfg(test)]
impl AllowPrivateLnurlHostsGuard {
    pub fn enable() -> Self {
        let previous = ALLOW_PRIVATE_LNURL_HOSTS.swap(true, Ordering::SeqCst);
        Self { previous }
    }

    /// Hold the policy-test lock so concurrent tests cannot flip the flag.
    pub async fn lock_policy() -> tokio::sync::MutexGuard<'static, ()> {
        LNURL_HOST_POLICY_TEST_LOCK.lock().await
    }

    /// Sync variant for non-async unit tests.
    pub fn lock_policy_sync() -> tokio::sync::MutexGuard<'static, ()> {
        LNURL_HOST_POLICY_TEST_LOCK.blocking_lock()
    }
}

#[cfg(test)]
impl Drop for AllowPrivateLnurlHostsGuard {
    fn drop(&mut self) {
        ALLOW_PRIVATE_LNURL_HOSTS.store(self.previous, Ordering::SeqCst);
    }
}

/// True for IPv4 CGNAT / carrier-grade NAT (`100.64.0.0/10`, RFC 6598).
/// Manual check: `Ipv4Addr::is_shared` is still unstable on stable Rust.
fn ipv4_is_cgnat(v4: std::net::Ipv4Addr) -> bool {
    let octets = v4.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

fn ipv4_is_always_forbidden(v4: std::net::Ipv4Addr) -> bool {
    v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_link_local()
        || v4.is_documentation()
        || ipv4_is_cgnat(v4)
}

/// RFC 6052 well-known NAT64 prefix `64:ff9b::/96`.
fn ipv6_is_nat64_well_known(v6: std::net::Ipv6Addr) -> bool {
    let s = v6.segments();
    s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0
}

/// True for destinations the daemon must never fetch for LNURL (SSRF policy).
/// Also applied to user-chosen Cashu mints on an open node
/// (`cashu::mint_policy`).
///
/// Always rejects link-local, CGNAT (RFC 6598 `100.64.0.0/10`), NAT64 well-known
/// prefix (RFC 6052 `64:ff9b::/96`), unspecified, multicast, broadcast, and
/// documentation ranges. Loopback and RFC1918 private are rejected unless
/// the test-only allow flag is set (local mock servers).
pub(crate) fn ip_is_forbidden(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            if ipv4_is_always_forbidden(v4) {
                return true;
            }
            if private_hosts_allowed() {
                return false;
            }
            v4.is_loopback() || v4.is_private()
        }
        IpAddr::V6(v6) => {
            if v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unicast_link_local()
                || ipv6_is_nat64_well_known(v6)
                || v6
                    .to_ipv4_mapped()
                    .map(ipv4_is_always_forbidden)
                    .unwrap_or(false)
            {
                return true;
            }
            if private_hosts_allowed() {
                return false;
            }
            v6.is_loopback()
                || v6.is_unique_local()
                || v6
                    .to_ipv4_mapped()
                    .map(|v4| v4.is_loopback() || v4.is_private())
                    .unwrap_or(false)
        }
    }
}

/// LUD-01 scheme rule for every LNURL fetch: `https`, or plain `http` only
/// for a Tor `.onion` host.
///
/// Clearnet `http` is refused: the callback response carries the invoice we
/// pay, and the later checks (chain, CLTV, expiry, amount) cannot tell an
/// attacker's invoice for the same amount from the real one. Anyone on the
/// path could otherwise swap it and take the buyer's payout.
///
/// Under the test-only private-host flag, plain `http` is also allowed to
/// `localhost` and loopback/private IP literals, so local mock LNURL servers
/// keep working. A public `http` host stays refused even then; production
/// never sets the flag.
fn lnurl_scheme_allowed(url: &Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => {
            let Some(host) = url.host_str() else {
                return false;
            };
            let host = host.trim_end_matches('.');
            if host.ends_with(".onion") {
                return true;
            }
            private_hosts_allowed()
                && match host.trim_matches(['[', ']']).parse::<IpAddr>() {
                    Ok(IpAddr::V4(v4)) => v4.is_loopback() || v4.is_private(),
                    Ok(IpAddr::V6(v6)) => v6.is_loopback() || v6.is_unique_local(),
                    Err(_) => host == "localhost",
                }
        }
        _ => false,
    }
}

/// Resolve `url`'s host and reject non-public destinations before any GET.
///
/// DNS lookup is bounded by [`LNURL_DNS_TIMEOUT`]. Returns one checked
/// [`SocketAddr`] suitable for reqwest's `.resolve` pin so the subsequent
/// request cannot be steered to a different IP via DNS rebinding.
async fn assert_url_host_safe(url: &Url) -> Result<SocketAddr, MostroError> {
    if !lnurl_scheme_allowed(url) {
        warn!("LNURL refused: clearnet URLs must use https");
        return Err(MostroInternalErr(ServiceError::LnAddressParseError));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(MostroInternalErr(ServiceError::LnAddressParseError));
    }
    let host = url
        .host_str()
        .ok_or(MostroInternalErr(ServiceError::LnAddressParseError))?;
    let port = url
        .port_or_known_default()
        .ok_or(MostroInternalErr(ServiceError::LnAddressParseError))?;

    // IP literal: no DNS; check and pin directly.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if ip_is_forbidden(ip) {
            warn!("LNURL refused forbidden IP literal destination");
            return Err(MostroInternalErr(ServiceError::LnAddressParseError));
        }
        return Ok(SocketAddr::new(ip, port));
    }

    let addrs: Vec<SocketAddr> =
        tokio::time::timeout(LNURL_DNS_TIMEOUT, tokio::net::lookup_host((host, port)))
            .await
            .map_err(|_| MostroInternalErr(ServiceError::NoAPIResponse))?
            .map_err(|_| MostroInternalErr(ServiceError::NoAPIResponse))?
            .collect();
    if addrs.is_empty() {
        return Err(MostroInternalErr(ServiceError::NoAPIResponse));
    }
    if addrs.iter().any(|a| ip_is_forbidden(a.ip())) {
        warn!("LNURL refused destination that resolves to a forbidden address");
        return Err(MostroInternalErr(ServiceError::LnAddressParseError));
    }
    // Prefer IPv4 when both families are present so a hostname like
    // `localhost` pins to the address local mock servers typically bind.
    Ok(addrs
        .iter()
        .find(|a| a.is_ipv4())
        .copied()
        .unwrap_or(addrs[0]))
}

/// GET an LNURL URL after host policy checks, with DNS pin and no redirects.
///
/// Applies [`LNURL_CONNECT_TIMEOUT`] and [`LNURL_REQUEST_TIMEOUT`]. Callers
/// must only pass URLs produced by [`extract_lnurl`] or [`build_callback_url`]
/// (scheme already constrained to http/https); the LUD-01 rule (`https`, or
/// `http` only for `.onion`) is then enforced here via [`assert_url_host_safe`].
async fn lnurl_get(url: Url) -> Result<reqwest::Response, MostroError> {
    let pinned = assert_url_host_safe(&url).await?;
    let host = url
        .host_str()
        .ok_or(MostroInternalErr(ServiceError::LnAddressParseError))?
        .to_string();

    let client = Client::builder()
        .timeout(LNURL_REQUEST_TIMEOUT)
        .connect_timeout(LNURL_CONNECT_TIMEOUT)
        .user_agent(concat!("mostro/", env!("CARGO_PKG_VERSION")))
        .redirect(Policy::none())
        // A system proxy (`HTTP_PROXY`, …) would resolve the host itself and
        // bypass the pinned address.
        .no_proxy()
        .resolve(&host, pinned)
        .build()
        .map_err(|_| MostroInternalErr(ServiceError::NoAPIResponse))?;

    client
        .get(url)
        .send()
        .await
        .map_err(|_| MostroInternalErr(ServiceError::NoAPIResponse))
}

/// The response body as text, or `None` if it is longer than
/// [`MAX_LNURL_RESPONSE_BYTES`] (announced by `content-length` or not) or the
/// stream fails. Callers map `None` as they mapped a failed `text()` read.
async fn read_body_capped(mut response: reqwest::Response) -> Option<String> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_LNURL_RESPONSE_BYTES as u64)
    {
        warn!("LNURL response refused: announced body over {MAX_LNURL_RESPONSE_BYTES} bytes");
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len() + chunk.len() > MAX_LNURL_RESPONSE_BYTES {
            warn!("LNURL response refused: body over {MAX_LNURL_RESPONSE_BYTES} bytes");
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(String::from_utf8_lossy(&body).into_owned())
}

/// Parse a Lightning Address or bech32 LNURL into an http(s) [`Url`].
///
/// - Lightning Address (`user@domain`) → well-known LNURL-pay endpoint URL
/// - Bech32 LNURL (`lnurl1…`) → decoded callback/pay URL
///
/// Returns a parsed [`Url`] (not a raw string) so callers can pass it straight
/// to [`lnurl_get`] without a second parse. Scheme must be `http`/`https`
/// here; the stricter LUD-01 rule (clearnet `http` refused) and host/IP
/// safety are enforced later by [`lnurl_get`].
///
/// # Errors
///
/// * `LnAddressParseError` — malformed address, bad bech32, or non-http(s) scheme
async fn extract_lnurl(address: &str) -> Result<Url, MostroError> {
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
    let parsed =
        Url::parse(&url).map_err(|_| MostroInternalErr(ServiceError::LnAddressParseError))?;
    if !crate::util::is_http_or_https(&parsed) {
        return Err(MostroInternalErr(ServiceError::LnAddressParseError));
    }
    Ok(parsed)
}

/// Probe whether a Lightning Address or LNURL-pay endpoint advertises
/// `tag: payRequest`.
///
/// Uses [`lnurl_get`] (host policy, DNS pin, timeouts, no redirects). Does not
/// request an invoice — only the LNURL-pay metadata document.
pub async fn ln_exists(address: &str) -> Result<(), MostroError> {
    let url = extract_lnurl(address).await?;
    let res = lnurl_get(url).await?;
    let status = res.status();
    if status.is_success() {
        let body = read_body_capped(res)
            .await
            .ok_or(MostroInternalErr(ServiceError::NoAPIResponse))?;
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
/// Scheme must be `http`/`https` here. The stricter LUD-01 rule (`https`, or
/// `http` only for `.onion`) and host/IP safety (reject private, loopback,
/// link-local, etc., and DNS-pin the request) are enforced by [`lnurl_get`]
/// before the callback is fetched.
fn build_callback_url(
    callback: &str,
    amount_msat: u64,
    comment: Option<&str>,
    comment_allowed: usize,
) -> Result<Url, MostroError> {
    let mut url =
        Url::parse(callback).map_err(|_| MostroInternalErr(ServiceError::LnAddressParseError))?;
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
///
/// Both the initial LNURL-pay document and the callback fetch go through
/// [`lnurl_get`] (SSRF host policy, DNS pin, timeouts, no redirects). The
/// callback URL is rebuilt with [`build_callback_url`] so `amount`/`comment`
/// are proper query params.
///
/// Permanent LNURL-level failures (`status: ERROR`, missing `payRequest`,
/// amount out of range, empty `pr`) return `Err` so callers do not treat
/// them as soft empty invoices. Callers that pay the returned `pr` (e.g.
/// `do_payment`) should still decode it as BOLT11 before submitting to LND.
pub async fn resolv_ln_address(
    address: &str,
    amount: u64,
    comment: Option<&str>,
) -> Result<String, MostroError> {
    let url = extract_lnurl(address).await?;
    let amount_msat = amount
        .checked_mul(1000)
        .ok_or(MostroInternalErr(ServiceError::WrongAmountError))?;

    let res = lnurl_get(url).await?;
    if !res.status().is_success() {
        return Err(MostroInternalErr(ServiceError::MalformedAPIRes));
    }
    let body = read_body_capped(res)
        .await
        .ok_or(MostroInternalErr(ServiceError::MessageSerializationError))?;
    let body: Value = serde_json::from_str(&body)
        .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
    if body["status"].as_str() == Some("ERROR") {
        let reason_len = body["reason"].as_str().map(str::len).unwrap_or(0);
        error!("LNURL address rejected by server (reason length: {reason_len} bytes)");
        return Err(MostroInternalErr(ServiceError::LnAddressParseError));
    }
    let tag = body["tag"].as_str().unwrap_or("");
    if tag != "payRequest" {
        return Err(MostroInternalErr(ServiceError::LnAddressParseError));
    }
    let min = body["minSendable"].as_u64().unwrap_or(0);
    let max = body["maxSendable"].as_u64().unwrap_or(0);
    if min > amount_msat || max < amount_msat {
        return Err(MostroInternalErr(ServiceError::LnAddressParseError));
    }
    let callback = body["callback"].as_str().unwrap_or("");
    let comment_allowed = body["commentAllowed"].as_u64().unwrap_or(0) as usize;
    let callback = build_callback_url(callback, amount_msat, comment, comment_allowed)?;
    let res = lnurl_get(callback).await?;
    if !res.status().is_success() {
        return Err(MostroInternalErr(ServiceError::MalformedAPIRes));
    }
    let body = read_body_capped(res)
        .await
        .ok_or(MostroInternalErr(ServiceError::MessageSerializationError))?;
    let body: Value = serde_json::from_str(&body)
        .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
    if body["status"].as_str() == Some("ERROR") {
        let reason_len = body["reason"].as_str().map(str::len).unwrap_or(0);
        error!("LNURL callback rejected by server (reason length: {reason_len} bytes)");
        return Err(MostroInternalErr(ServiceError::LnAddressParseError));
    }
    let pr = body["pr"].as_str().unwrap_or("");
    if pr.is_empty() {
        return Err(MostroInternalErr(ServiceError::LnAddressParseError));
    }
    Ok(pr.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::StatusCode, routing::get, Json, Router};
    use serde_json::json;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    #[test]
    fn ip_is_forbidden_rejects_loopback_private_and_link_local() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy_sync();
        let _guard = AllowPrivateLnurlHostsGuard {
            previous: ALLOW_PRIVATE_LNURL_HOSTS.swap(false, Ordering::SeqCst),
        };
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(
            169, 254, 169, 254
        ))));
        // CGNAT / shared address space (RFC 6598).
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(
            100, 127, 255, 254
        ))));
        // NAT64 well-known prefix embedding 8.8.8.8 (RFC 6052).
        assert!(ip_is_forbidden(IpAddr::V6(Ipv6Addr::new(
            0x64, 0xff9b, 0, 0, 0, 0, 0x0808, 0x0808
        ))));
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(ip_is_forbidden(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[test]
    fn ip_is_forbidden_test_guard_allows_loopback_but_not_link_local() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy_sync();
        let _guard = AllowPrivateLnurlHostsGuard::enable();
        assert!(!ip_is_forbidden(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(!ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        // Cloud-metadata style link-local stays forbidden even for local mocks.
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(
            169, 254, 169, 254
        ))));
        // CGNAT and NAT64 stay forbidden under the test guard too.
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(100, 64, 1, 1))));
        assert!(ip_is_forbidden(IpAddr::V6(Ipv6Addr::new(
            0x64, 0xff9b, 0, 0, 0, 0, 0xc0a8, 0x0001
        ))));
    }

    #[tokio::test]
    async fn assert_url_host_safe_rejects_loopback_literal() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy().await;
        let _guard = AllowPrivateLnurlHostsGuard {
            previous: ALLOW_PRIVATE_LNURL_HOSTS.swap(false, Ordering::SeqCst),
        };
        // https so the scheme check passes and the IP policy does the refusing.
        let url = Url::parse("https://127.0.0.1/cb").unwrap();
        assert!(matches!(
            assert_url_host_safe(&url).await,
            Err(MostroInternalErr(ServiceError::LnAddressParseError))
        ));
    }

    #[tokio::test]
    async fn assert_url_host_safe_rejects_userinfo() {
        let url = Url::parse("https://user:pass@example.com/cb").unwrap();
        assert!(assert_url_host_safe(&url).await.is_err());
    }

    /// Strict policy: clearnet `http` is refused (a MITM could swap the
    /// invoice), `https` and LUD-01's `http` `.onion` exception pass.
    #[test]
    fn lnurl_scheme_allowed_requires_https_on_clearnet() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy_sync();
        let _guard = AllowPrivateLnurlHostsGuard {
            previous: ALLOW_PRIVATE_LNURL_HOSTS.swap(false, Ordering::SeqCst),
        };
        let allowed = |u: &str| lnurl_scheme_allowed(&Url::parse(u).unwrap());

        assert!(allowed("https://pay.example.com/.well-known/lnurlp/alice"));
        assert!(allowed("https://8.8.8.8/cb"));
        assert!(allowed("http://abcdefghijklmnop.onion/cb"));
        assert!(allowed("http://ABCDEFGHIJKLMNOP.ONION./cb"));

        assert!(!allowed("http://pay.example.com/cb"));
        assert!(!allowed("http://8.8.8.8/cb"));
        assert!(!allowed("http://onion.example.com/cb"));
        assert!(!allowed("http://example.onion.evil.com/cb"));
        // Local hosts need the test flag; production never has it.
        assert!(!allowed("http://127.0.0.1/cb"));
        assert!(!allowed("http://localhost/cb"));
        assert!(!allowed("ftp://pay.example.com/cb"));
    }

    /// The test flag lets local mock servers use `http`, but never a public
    /// host, so payout tests can still exercise the clearnet refusal.
    #[test]
    fn lnurl_scheme_allowed_test_guard_only_lifts_local_hosts() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy_sync();
        let _guard = AllowPrivateLnurlHostsGuard::enable();
        let allowed = |u: &str| lnurl_scheme_allowed(&Url::parse(u).unwrap());

        assert!(allowed("http://127.0.0.1:8080/cb"));
        assert!(allowed("http://10.0.0.1/cb"));
        assert!(allowed("http://[::1]:8080/cb"));
        assert!(allowed("http://localhost:8080/cb"));

        assert!(!allowed("http://pay.example.com/cb"));
        assert!(!allowed("http://8.8.8.8/cb"));
    }

    /// The scheme check runs before the IP policy: a public IP literal passes
    /// the host checks, so only the scheme can refuse it.
    #[tokio::test]
    async fn assert_url_host_safe_rejects_clearnet_http() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy().await;
        let _guard = AllowPrivateLnurlHostsGuard {
            previous: ALLOW_PRIVATE_LNURL_HOSTS.swap(false, Ordering::SeqCst),
        };
        let http = Url::parse("http://8.8.8.8/cb").unwrap();
        assert!(matches!(
            assert_url_host_safe(&http).await,
            Err(MostroInternalErr(ServiceError::LnAddressParseError))
        ));
        let https = Url::parse("https://8.8.8.8/cb").unwrap();
        assert!(assert_url_host_safe(&https).await.is_ok());
    }

    /// A bech32 LNURL decoding to clearnet `http` is refused when the buyer
    /// submits it, not only at payout.
    #[tokio::test]
    async fn ln_exists_refuses_clearnet_http_lnurl() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy().await;
        let _guard = AllowPrivateLnurlHostsGuard {
            previous: ALLOW_PRIVATE_LNURL_HOSTS.swap(false, Ordering::SeqCst),
        };
        let encoded = LnUrl {
            url: "http://8.8.8.8/.well-known/lnurlp/alice".to_string(),
        }
        .encode();
        assert!(matches!(
            ln_exists(&encoded).await,
            Err(MostroInternalErr(ServiceError::LnAddressParseError))
        ));
    }

    #[tokio::test]
    async fn extract_lnurl_decodes_bech32_lnurl() {
        let url = "https://example.com/.well-known/lnurlp/alice";
        let encoded = LnUrl {
            url: url.to_string(),
        }
        .encode();
        assert!(encoded.to_lowercase().starts_with("lnurl1"));

        let extracted = extract_lnurl(&encoded).await.expect("valid LNURL decodes");
        assert_eq!(extracted.to_string(), url);
    }

    #[tokio::test]
    async fn extract_lnurl_rejects_malformed_bech32() {
        assert!(extract_lnurl("lnurl1notvalidbech32").await.is_err());
    }

    #[tokio::test]
    async fn extract_lnurl_rejects_non_http_scheme() {
        let encoded = LnUrl {
            url: "javascript:alert(1)".to_string(),
        }
        .encode();
        assert!(extract_lnurl(&encoded).await.is_err());
    }

    #[tokio::test]
    async fn extract_lnurl_builds_wellknown_url_for_lightning_address() {
        let extracted = extract_lnurl("alice@127.0.0.1")
            .await
            .expect("lightning address parses");
        assert_eq!(
            extracted.to_string(),
            "http://127.0.0.1:8080/.well-known/lnurlp/alice"
        );
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

    /// Report regression: attacker LNURL (A) is reachable (test guard allows
    /// loopback), but its `callback` points at link-local metadata
    /// (`169.254.169.254`). Resolution must fail at the callback host check;
    /// the internal target must never be contacted.
    #[tokio::test]
    async fn resolv_ln_address_refuses_link_local_callback() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy().await;
        let _guard = AllowPrivateLnurlHostsGuard::enable();

        let hits = Arc::new(Mutex::new(Vec::<String>::new()));
        // Bind a listener on an unused port only so we can detect accidental
        // contact if policy ever regresses to allowing link-local by rewriting
        // — the callback URL itself is the metadata IP literal.
        let hits_b = hits.clone();
        let app_b = Router::new().route(
            "/cb",
            get(move |method: axum::http::Method, uri: axum::http::Uri| {
                let hits = hits_b.clone();
                async move {
                    hits.lock().unwrap().push(format!(
                        "{method} {}?{}",
                        uri.path(),
                        uri.query().unwrap_or("")
                    ));
                    (StatusCode::OK, Json(json!({ "pr": "lnbc1" })))
                }
            }),
        );
        let listener_b = TcpListener::bind("127.0.0.1:0").await.unwrap();
        tokio::spawn(async move {
            axum::serve(listener_b, app_b).await.unwrap();
        });

        let app_a = Router::new().route(
            "/.well-known/lnurlp/alice",
            get(|| async {
                (
                    StatusCode::OK,
                    Json(json!({
                        "status": "OK",
                        "tag": "payRequest",
                        "callback": "https://169.254.169.254/latest/meta-data",
                        "minSendable": 1000,
                        "maxSendable": 100000000,
                        "metadata": "[]"
                    })),
                )
            }),
        );
        let listener_a = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port_a = listener_a.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener_a, app_a).await.unwrap();
        });

        let lnurl_string = LnUrl {
            url: format!("http://127.0.0.1:{port_a}/.well-known/lnurlp/alice"),
        }
        .encode();

        let result = resolv_ln_address(&lnurl_string, 100, None).await;
        // The IP policy refuses with `LnAddressParseError`; a request that got
        // past it would fail to connect with `NoAPIResponse` instead.
        assert!(
            matches!(
                result,
                Err(MostroInternalErr(ServiceError::LnAddressParseError))
            ),
            "link-local callback must be refused by the IP policy: {result:?}"
        );
        assert!(
            hits.lock().unwrap().is_empty(),
            "no request must reach any internal listener"
        );
    }

    /// MITM regression: the LNURL server answers with a clearnet `http`
    /// callback. The callback response carries the invoice we pay, so it
    /// must be refused before any request; without the scheme rule this
    /// tried to connect and failed with `NoAPIResponse` instead.
    #[tokio::test]
    async fn resolv_ln_address_refuses_clearnet_http_callback() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy().await;
        let _guard = AllowPrivateLnurlHostsGuard::enable();

        let app = Router::new().route(
            "/.well-known/lnurlp/alice",
            get(|| async {
                (
                    StatusCode::OK,
                    Json(json!({
                        "status": "OK",
                        "tag": "payRequest",
                        "callback": "http://8.8.8.8/cb",
                        "minSendable": 1000,
                        "maxSendable": 100000000,
                        "metadata": "[]"
                    })),
                )
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let lnurl_string = LnUrl {
            url: format!("http://127.0.0.1:{port}/.well-known/lnurlp/alice"),
        }
        .encode();

        let result = resolv_ln_address(&lnurl_string, 100, None).await;
        assert!(
            matches!(
                result,
                Err(MostroInternalErr(ServiceError::LnAddressParseError))
            ),
            "clearnet http callback must be refused: {result:?}"
        );
    }

    /// Strict policy (no test guard): a bech32 LNURL aimed at loopback is
    /// refused before the first GET.
    #[tokio::test]
    async fn resolv_ln_address_refuses_initial_loopback_lnurl() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy().await;
        let _policy = AllowPrivateLnurlHostsGuard {
            previous: ALLOW_PRIVATE_LNURL_HOSTS.swap(false, Ordering::SeqCst),
        };

        let hits = Arc::new(Mutex::new(0u32));
        let hits_a = hits.clone();
        let app_a = Router::new().route(
            "/.well-known/lnurlp/alice",
            get(move || {
                let hits = hits_a.clone();
                async move {
                    *hits.lock().unwrap() += 1;
                    (
                        StatusCode::OK,
                        Json(json!({
                            "status": "OK",
                            "tag": "payRequest",
                            "callback": "http://127.0.0.1:9/cb",
                            "minSendable": 1000,
                            "maxSendable": 100000000,
                            "metadata": "[]"
                        })),
                    )
                }
            }),
        );
        let listener_a = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port_a = listener_a.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener_a, app_a).await.unwrap();
        });

        let lnurl_string = LnUrl {
            url: format!("https://127.0.0.1:{port_a}/.well-known/lnurlp/alice"),
        }
        .encode();

        let result = resolv_ln_address(&lnurl_string, 100, None).await;
        // https so the scheme check passes and the IP policy does the refusing.
        // Past the policy, the TLS handshake with this plain-http listener would
        // fail with `NoAPIResponse` instead.
        assert!(
            matches!(
                result,
                Err(MostroInternalErr(ServiceError::LnAddressParseError))
            ),
            "loopback LNURL must be refused by the IP policy: {result:?}"
        );
        assert_eq!(
            *hits.lock().unwrap(),
            0,
            "daemon must not contact the loopback LNURL host"
        );
    }

    #[tokio::test]
    async fn resolv_ln_address_returns_err_on_lnurl_error_status() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy().await;
        let _guard = AllowPrivateLnurlHostsGuard::enable();

        let app = Router::new().route(
            "/.well-known/lnurlp/alice",
            get(|| async {
                (
                    StatusCode::OK,
                    Json(json!({
                        "status": "ERROR",
                        "reason": "nope"
                    })),
                )
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let lnurl_string = LnUrl {
            url: format!("http://127.0.0.1:{port}/.well-known/lnurlp/alice"),
        }
        .encode();

        let result = resolv_ln_address(&lnurl_string, 100, None).await;
        assert!(
            matches!(
                result,
                Err(MostroInternalErr(ServiceError::LnAddressParseError))
            ),
            "LNURL ERROR status must be Err, not Ok(\"\"): {result:?}"
        );
    }

    #[tokio::test]
    async fn resolv_ln_address_returns_err_on_non_payrequest_tag() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy().await;
        let _guard = AllowPrivateLnurlHostsGuard::enable();

        let app = Router::new().route(
            "/.well-known/lnurlp/alice",
            get(|| async {
                (
                    StatusCode::OK,
                    Json(json!({
                        "status": "OK",
                        "tag": "withdrawRequest"
                    })),
                )
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let lnurl_string = LnUrl {
            url: format!("http://127.0.0.1:{port}/.well-known/lnurlp/alice"),
        }
        .encode();

        let result = resolv_ln_address(&lnurl_string, 100, None).await;
        assert!(
            matches!(
                result,
                Err(MostroInternalErr(ServiceError::LnAddressParseError))
            ),
            "non-payRequest tag must be Err: {result:?}"
        );
    }

    /// Env var that arms [`lnurl_get_reaches_the_pinned_host_in_a_proxied_env`]
    /// in the child process spawned by
    /// [`lnurl_get_ignores_system_proxies`].
    const PROXY_PROBE_ENV: &str = "MOSTRO_LNURL_PROXY_PROBE";

    /// Child half of [`lnurl_get_ignores_system_proxies`]: runs only in a
    /// process whose proxy env vars point at a dead proxy. `lnurl_get` must
    /// still reach the local server it pinned.
    #[tokio::test]
    #[ignore = "spawned by lnurl_get_ignores_system_proxies with a proxied env"]
    async fn lnurl_get_reaches_the_pinned_host_in_a_proxied_env() {
        if std::env::var(PROXY_PROBE_ENV).is_err() {
            return;
        }
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy().await;
        let _guard = AllowPrivateLnurlHostsGuard::enable();
        let app = Router::new().route("/probe", get(|| async { StatusCode::OK }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = Url::parse(&format!("http://localhost:{port}/probe")).unwrap();
        let response = lnurl_get(url)
            .await
            .expect("lnurl_get must ignore the proxy and reach the pinned host");
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// A system proxy resolves the host itself, which would bypass the
    /// checked address `lnurl_get` pins (DNS rebinding). reqwest reads
    /// `HTTP_PROXY` & co. when a client is built, so the check runs in a
    /// child test process: setting them here would race with every other
    /// test that builds a reqwest client.
    #[test]
    fn lnurl_get_ignores_system_proxies() {
        let dead_proxy = "http://127.0.0.1:9";
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "lnurl::tests::lnurl_get_reaches_the_pinned_host_in_a_proxied_env",
            ])
            .env(PROXY_PROBE_ENV, "1")
            .env("HTTP_PROXY", dead_proxy)
            .env("http_proxy", dead_proxy)
            .env("HTTPS_PROXY", dead_proxy)
            .env("ALL_PROXY", dead_proxy)
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .output()
            .expect("spawn the test binary");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "child test failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    const BODY_CAP: usize = MAX_LNURL_RESPONSE_BYTES;

    /// The two ways a server can send a body of `body.len()` bytes: announced
    /// by `content-length`, or streamed until the connection closes.
    fn http_200_responses(body: &str) -> [String; 2] {
        [
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            ),
            format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{body}"),
        ]
    }

    /// A JSON object made of `fields` plus a `pad` string, exactly `len` bytes.
    fn json_padded_to(fields: &str, len: usize) -> String {
        let head = format!("{{{fields},\"pad\":\"");
        let tail = "\"}";
        assert!(len >= head.len() + tail.len(), "padding target too small");
        format!("{head}{}{tail}", "a".repeat(len - head.len() - tail.len()))
    }

    /// A loopback server that answers its n-th connection with the n-th
    /// response built by `responses` (which gets the server's own address,
    /// for callback URLs). The client may hang up early on an oversized body.
    async fn serve_in_order(responses: impl FnOnce(SocketAddr) -> Vec<String>) -> SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let responses = responses(addr);
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        addr
    }

    fn lnurl_for(addr: SocketAddr, path: &str) -> String {
        LnUrl {
            url: format!("http://{addr}{path}"),
        }
        .encode()
    }

    #[tokio::test]
    async fn ln_exists_refuses_a_body_over_the_cap() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy().await;
        let _guard = AllowPrivateLnurlHostsGuard::enable();
        let body = json_padded_to(r#""tag":"payRequest""#, BODY_CAP + 1);
        for response in http_200_responses(&body) {
            let head = response[..response.find("\r\n\r\n").unwrap()].to_string();
            let addr = serve_in_order(|_| vec![response]).await;
            let result = ln_exists(&lnurl_for(addr, "/.well-known/lnurlp/alice")).await;
            assert!(
                result.is_err(),
                "{head:?}: a body over the cap must be refused"
            );
        }
    }

    #[tokio::test]
    async fn ln_exists_accepts_a_body_exactly_at_the_cap() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy().await;
        let _guard = AllowPrivateLnurlHostsGuard::enable();
        let body = json_padded_to(r#""tag":"payRequest""#, BODY_CAP);
        for response in http_200_responses(&body) {
            let head = response[..response.find("\r\n\r\n").unwrap()].to_string();
            let addr = serve_in_order(|_| vec![response]).await;
            let result = ln_exists(&lnurl_for(addr, "/.well-known/lnurlp/alice")).await;
            assert!(
                result.is_ok(),
                "{head:?}: a body at the cap must be read: {result:?}"
            );
        }
    }

    /// A body announced over the cap is refused before any of it is read: the
    /// server here sends a valid document but never closes, so waiting for the
    /// announced bytes would last until `LNURL_REQUEST_TIMEOUT`.
    #[tokio::test]
    async fn ln_exists_refuses_an_announced_oversize_without_waiting_for_it() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy().await;
        let _guard = AllowPrivateLnurlHostsGuard::enable();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let body = r#"{"tag":"payRequest"}"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                MAX_LNURL_RESPONSE_BYTES + 1
            );
            let _ = socket.write_all(format!("{head}{body}").as_bytes()).await;
            tokio::time::sleep(LNURL_REQUEST_TIMEOUT * 2).await;
        });

        let started = std::time::Instant::now();
        let result = ln_exists(&lnurl_for(addr, "/.well-known/lnurlp/alice")).await;
        assert!(result.is_err(), "an announced oversize must be refused");
        assert!(
            started.elapsed() < LNURL_REQUEST_TIMEOUT - Duration::from_secs(1),
            "refused only after {:?}: the announced length was not checked first",
            started.elapsed()
        );
    }

    /// The LNURL-pay document (first fetch of `resolv_ln_address`) over the cap.
    #[tokio::test]
    async fn resolv_ln_address_refuses_a_pay_document_over_the_cap() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy().await;
        let _guard = AllowPrivateLnurlHostsGuard::enable();
        for variant in 0..2 {
            let addr = serve_in_order(|addr| {
                let document = json_padded_to(
                    &format!(
                        r#""tag":"payRequest","callback":"http://{addr}/cb","minSendable":1000,"maxSendable":1000000000"#
                    ),
                    BODY_CAP + 1,
                );
                let callback = r#"{"pr":"lnbc1fake","routes":[]}"#;
                vec![
                    http_200_responses(&document)[variant].clone(),
                    http_200_responses(callback)[0].clone(),
                ]
            })
            .await;
            let result =
                resolv_ln_address(&lnurl_for(addr, "/.well-known/lnurlp/alice"), 100, None).await;
            assert!(
                result.is_err(),
                "variant {variant}: a pay document over the cap must be refused: {result:?}"
            );
        }
    }

    /// The callback reply (second fetch of `resolv_ln_address`) over the cap.
    #[tokio::test]
    async fn resolv_ln_address_refuses_a_callback_reply_over_the_cap() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy().await;
        let _guard = AllowPrivateLnurlHostsGuard::enable();
        for variant in 0..2 {
            let addr = serve_in_order(|addr| {
                let document = format!(
                    r#"{{"tag":"payRequest","callback":"http://{addr}/cb","minSendable":1000,"maxSendable":1000000000}}"#
                );
                let callback = json_padded_to(r#""pr":"lnbc1fake""#, BODY_CAP + 1);
                vec![
                    http_200_responses(&document)[0].clone(),
                    http_200_responses(&callback)[variant].clone(),
                ]
            })
            .await;
            let result =
                resolv_ln_address(&lnurl_for(addr, "/.well-known/lnurlp/alice"), 100, None).await;
            assert!(
                result.is_err(),
                "variant {variant}: a callback reply over the cap must be refused: {result:?}"
            );
        }
    }
}
