//! The HTTP transport mostrod talks to Cashu mints over (issue #1046).
//!
//! cdk's default transport resolves the mint's host on every request. On an
//! open node (`mint_urls = []`) the maker picks the mint, so mostrod checks
//! that the host resolves to a public address first — but a name server that
//! answers differently on the next lookup (DNS rebinding) would then steer
//! the request to a private one. [`MintTransport::pinned`] closes that gap:
//! the host is pinned to the address that passed the check, the same way
//! `lnurl_get` pins LNURL requests. Redirects are never followed.
//!
//! Responses are handled exactly as cdk's own transport handles them: a
//! non-2xx body that parses as a Cashu `ErrorResponse` becomes that error,
//! anything else an `HttpError` with the status.

use std::net::SocketAddr;

use async_trait::async_trait;
use cdk::error::{Error, ErrorResponse};
use cdk::nuts::AuthToken;
use cdk::wallet::HttpTransport;
use reqwest::redirect::Policy;
use reqwest::RequestBuilder;
use serde::de::DeserializeOwned;
use serde::Serialize;

/// A no-redirect HTTP transport, optionally pinned to one checked address.
#[derive(Debug, Clone)]
pub struct MintTransport {
    inner: reqwest::Client,
}

impl Default for MintTransport {
    /// Unpinned: the host is resolved per request. For mints the operator
    /// listed in `mint_urls`.
    fn default() -> Self {
        Self::build(None).expect("a no-redirect reqwest client always builds")
    }
}

impl MintTransport {
    /// Send every request for `host` to `addr`, never re-resolving the name.
    /// The URL's port is still the one used, as with reqwest's `resolve`.
    pub fn pinned(host: &str, addr: SocketAddr) -> Result<Self, Error> {
        Self::build(Some((host, addr)))
    }

    fn build(pin: Option<(&str, SocketAddr)>) -> Result<Self, Error> {
        let mut builder = reqwest::Client::builder()
            .redirect(Policy::none())
            .user_agent(concat!("mostro/", env!("CARGO_PKG_VERSION")));
        if let Some((host, addr)) = pin {
            builder = builder.resolve(host, addr);
        }
        let inner = builder
            .build()
            .map_err(|e| Error::HttpError(None, e.to_string()))?;
        Ok(Self { inner })
    }

    async fn send<R: DeserializeOwned>(
        request: RequestBuilder,
        auth: Option<AuthToken>,
    ) -> Result<R, Error> {
        let request = match auth {
            Some(auth) => request.header(auth.header_key(), auth.to_string()),
            None => request,
        };
        let response = request
            .send()
            .await
            .map_err(|e| Error::HttpError(None, e.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| Error::HttpError(None, e.to_string()))?;

        if !(200..300).contains(&status) {
            if let Ok(err_resp) = serde_json::from_str::<ErrorResponse>(&body) {
                return Err(err_resp.into());
            }
            return Err(Error::HttpError(Some(status), body));
        }
        serde_json::from_str::<R>(&body).map_err(|_| match ErrorResponse::from_json(&body) {
            Ok(err_resp) => err_resp.into(),
            Err(err) => err.into(),
        })
    }
}

#[async_trait]
impl HttpTransport for MintTransport {
    fn with_proxy(
        &mut self,
        _proxy: reqwest::Url,
        _host_matcher: Option<&str>,
        _accept_invalid_certs: bool,
    ) -> Result<(), Error> {
        Err(Error::Custom(
            "mostrod's mint transport does not support proxies".to_string(),
        ))
    }

    async fn http_get<R>(&self, url: reqwest::Url, auth: Option<AuthToken>) -> Result<R, Error>
    where
        R: DeserializeOwned,
    {
        Self::send(self.inner.get(url.as_str()), auth).await
    }

    async fn http_post<P, R>(
        &self,
        url: reqwest::Url,
        auth: Option<AuthToken>,
        payload: &P,
    ) -> Result<R, Error>
    where
        P: Serialize + ?Sized + Send + Sync,
        R: DeserializeOwned,
    {
        Self::send(self.inner.post(url.as_str()).json(payload), auth).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serve one canned HTTP response on a loopback port; return its address.
    async fn serve_once(response: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await;
            socket.write_all(response.as_bytes()).await.unwrap();
            let _ = socket.shutdown().await;
        });
        addr
    }

    #[tokio::test]
    async fn pinned_transport_sends_the_host_to_the_pinned_address() {
        // `mint.invalid` never resolves (RFC 6761), so reaching the server at
        // all proves the request went to the pinned address, not to DNS.
        let addr = serve_once(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}",
        )
        .await;
        let transport = MintTransport::pinned("mint.invalid", addr).unwrap();
        let url =
            reqwest::Url::parse(&format!("http://mint.invalid:{}/v1/info", addr.port())).unwrap();

        let body: serde_json::Value = transport.http_get(url, None).await.expect("pinned GET");
        assert_eq!(body, serde_json::json!({ "ok": true }));
    }

    /// The full `CashuClient::connect` checks (mint info, NUTs, sat keyset)
    /// through a pinned transport, against a live mint reached by a name that
    /// never resolves.
    #[tokio::test]
    #[ignore = "requires a running mint and CASHU_TEST_MINT_URL"]
    async fn connect_pinned_reaches_a_live_mint_by_its_pinned_address() {
        let Ok(mint_url) = std::env::var("CASHU_TEST_MINT_URL") else {
            eprintln!("CASHU_TEST_MINT_URL unset; skipping");
            return;
        };
        let live = reqwest::Url::parse(&mint_url).expect("CASHU_TEST_MINT_URL must be a URL");
        let addr = live
            .socket_addrs(|| None)
            .expect("mint address")
            .into_iter()
            .next()
            .expect("mint address");
        let pinned_url = format!("{}://mint.invalid:{}", live.scheme(), addr.port());

        let client = crate::cashu::CashuClient::connect_pinned(&pinned_url, addr)
            .await
            .expect("connect through the pinned address");
        assert_eq!(
            client.mint_url().to_string().trim_end_matches('/'),
            pinned_url
        );
    }

    #[tokio::test]
    async fn transport_does_not_follow_redirects() {
        let addr = serve_once(
            "HTTP/1.1 302 Found\r\nlocation: http://169.254.169.254/latest\r\ncontent-length: 0\r\n\r\n",
        )
        .await;
        let transport = MintTransport::default();
        let url = reqwest::Url::parse(&format!("http://{addr}/v1/info")).unwrap();

        let err = transport
            .http_get::<serde_json::Value>(url, None)
            .await
            .expect_err("a redirect must not be followed");
        assert!(matches!(err, Error::HttpError(Some(302), _)), "{err:?}");
    }

    #[tokio::test]
    async fn transport_surfaces_a_cashu_error_response() {
        let addr = serve_once(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: 40\r\n\r\n{\"code\":11001,\"detail\":\"Token is spent\"}",
        )
        .await;
        let transport = MintTransport::default();
        let url = reqwest::Url::parse(&format!("http://{addr}/v1/checkstate")).unwrap();

        let err = transport
            .http_post::<_, serde_json::Value>(url, None, &serde_json::json!({}))
            .await
            .expect_err("a 400 with a Cashu error body is an error");
        assert!(!matches!(err, Error::HttpError(..)), "{err:?}");
    }
}
