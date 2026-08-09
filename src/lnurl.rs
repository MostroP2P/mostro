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

/// True for destinations the daemon must never fetch for LNURL (SSRF policy).
///
/// Always rejects link-local, unspecified, multicast, broadcast, and
/// documentation ranges. Loopback and RFC1918 private are rejected unless
/// the test-only allow flag is set (local mock servers).
fn ip_is_forbidden(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_link_local()
                || v4.is_documentation()
            {
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
                || v6
                    .to_ipv4_mapped()
                    .map(|v4| {
                        v4.is_unspecified()
                            || v4.is_broadcast()
                            || v4.is_multicast()
                            || v4.is_link_local()
                            || v4.is_documentation()
                    })
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

/// Resolve `url`'s host and reject non-public destinations before any GET.
///
/// Returns one checked [`SocketAddr`] suitable for reqwest's `.resolve` pin
/// so the subsequent request cannot be steered to a different IP via DNS
/// rebinding.
async fn assert_url_host_safe(url: &Url) -> Result<SocketAddr, MostroError> {
    if !crate::util::is_http_or_https(url) {
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

    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
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
        .resolve(&host, pinned)
        .build()
        .map_err(|_| MostroInternalErr(ServiceError::NoAPIResponse))?;

    client
        .get(url)
        .send()
        .await
        .map_err(|_| MostroInternalErr(ServiceError::NoAPIResponse))
}

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
/// (`ln_exists`, `resolv_ln_address`) does a GET against the result after
/// host/IP policy checks in [`lnurl_get`].
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
    let parsed =
        Url::parse(&url).map_err(|_| MostroInternalErr(ServiceError::LnAddressParseError))?;
    if !crate::util::is_http_or_https(&parsed) {
        return Err(MostroInternalErr(ServiceError::LnAddressParseError));
    }
    Ok(url)
}

pub async fn ln_exists(address: &str) -> Result<(), MostroError> {
    let url = extract_lnurl(address).await?;
    let url = Url::parse(&url).map_err(|_| MostroInternalErr(ServiceError::LnAddressParseError))?;
    let res = lnurl_get(url).await?;
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
/// Scheme must be `http`/`https`. Host/IP safety (reject private, loopback,
/// link-local, etc., and DNS-pin the request) is enforced by [`lnurl_get`]
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
/// Permanent LNURL-level failures (`status: ERROR`, missing `payRequest`,
/// amount out of range, empty `pr`) return `Err` so callers do not treat
/// them as soft empty invoices.
pub async fn resolv_ln_address(
    address: &str,
    amount: u64,
    comment: Option<&str>,
) -> Result<String, MostroError> {
    let url = extract_lnurl(address).await?;
    let url = Url::parse(&url).map_err(|_| MostroInternalErr(ServiceError::LnAddressParseError))?;
    let amount_msat = amount * 1000;

    let res = lnurl_get(url).await?;
    if !res.status().is_success() {
        return Err(MostroInternalErr(ServiceError::MalformedAPIRes));
    }
    let body = res
        .text()
        .await
        .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
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
    let body = res
        .text()
        .await
        .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
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
    }

    #[tokio::test]
    async fn assert_url_host_safe_rejects_loopback_literal() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy().await;
        let _guard = AllowPrivateLnurlHostsGuard {
            previous: ALLOW_PRIVATE_LNURL_HOSTS.swap(false, Ordering::SeqCst),
        };
        let url = Url::parse("http://127.0.0.1/cb").unwrap();
        assert!(assert_url_host_safe(&url).await.is_err());
    }

    #[tokio::test]
    async fn assert_url_host_safe_rejects_userinfo() {
        let url = Url::parse("https://user:pass@example.com/cb").unwrap();
        assert!(assert_url_host_safe(&url).await.is_err());
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
        assert_eq!(extracted, url);
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
                        "callback": "http://169.254.169.254/latest/meta-data",
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
        assert!(
            result.is_err(),
            "link-local callback must be refused: {result:?}"
        );
        assert!(
            hits.lock().unwrap().is_empty(),
            "no request must reach any internal listener"
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
            url: format!("http://127.0.0.1:{port_a}/.well-known/lnurlp/alice"),
        }
        .encode();

        let result = resolv_ln_address(&lnurl_string, 100, None).await;
        assert!(
            result.is_err(),
            "loopback LNURL must be refused before any GET: {result:?}"
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
}
