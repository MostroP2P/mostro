//! Bearer-token authentication for the admin gRPC surface.
//!
//! Every admin RPC is executed under the daemon's own Nostr identity (see
//! `crate::rpc::service`), and downstream authorization grants that identity
//! full privilege — `db::ensure_dispute_finalize_permission` short-circuits its
//! solver-category check for the daemon key. There is therefore no
//! message-level authorization left to fall back on: reaching the port is the
//! authorization, so the transport has to be the gate.
//!
//! Deliberately not rate-limited. `MIN_RPC_TOKEN_LEN` keeps the search space
//! far out of reach of online guessing, and tonic's [`Interceptor`] is
//! synchronous while [`crate::rpc::rate_limiter::RateLimiter`] is async, so
//! wiring one in would mean a second, parallel limiter for no security gain.

use secrecy::{ExposeSecret, SecretString};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tonic::service::Interceptor;
use tonic::{Request, Status};
use tracing::warn;

const AUTHORIZATION_HEADER: &str = "authorization";
/// RFC 7235 defines the auth-scheme as case-insensitive, and proxies do
/// normalize it, so the scheme is matched without regard to case. The
/// credential that follows is still compared byte for byte.
const BEARER_SCHEME: &str = "Bearer";

/// Rejects any request that does not carry the configured bearer token.
#[derive(Clone)]
pub struct BearerAuth {
    token: Arc<SecretString>,
}

impl BearerAuth {
    pub fn new(token: SecretString) -> Self {
        Self {
            token: Arc::new(token),
        }
    }
}

impl Interceptor for BearerAuth {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let presented = request
            .metadata()
            .get(AUTHORIZATION_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                let (scheme, credential) = value.split_once(' ')?;
                scheme
                    .eq_ignore_ascii_case(BEARER_SCHEME)
                    .then_some(credential)
            });

        match presented {
            Some(candidate) if credentials_match(candidate, self.token.expose_secret()) => {
                Ok(request)
            }
            // One message for every failure mode: a caller learns whether the
            // port is an admin RPC, never whether it guessed part of a token.
            _ => {
                match request.remote_addr() {
                    Some(addr) => warn!("Rejected unauthenticated admin RPC from {}", addr.ip()),
                    None => warn!("Rejected unauthenticated admin RPC from an unknown peer"),
                }
                Err(Status::unauthenticated("missing or invalid credentials"))
            }
        }
    }
}

/// Compare the presented credential against the configured one without leaking
/// how far the two matched.
///
/// `subtle` is used rather than a hand-written loop because only its
/// optimization barrier makes the constant-time property a guarantee the
/// compiler must honour. `ct_eq` on slices already answers `false` for
/// mismatched lengths, and the token's length is set by the operator's config
/// rather than being itself a secret.
fn credentials_match(presented: &str, configured: &str) -> bool {
    presented
        .as_bytes()
        .ct_eq(configured.as_bytes())
        .unwrap_u8()
        == 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::metadata::MetadataValue;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn interceptor() -> BearerAuth {
        BearerAuth::new(SecretString::from(TOKEN))
    }

    fn request_with_authorization(value: &str) -> Request<()> {
        let mut request = Request::new(());
        request.metadata_mut().insert(
            AUTHORIZATION_HEADER,
            MetadataValue::try_from(value).expect("header value is ASCII"),
        );
        request
    }

    #[test]
    fn accepts_the_configured_token() {
        let result = interceptor().call(request_with_authorization(&format!("Bearer {TOKEN}")));
        assert!(result.is_ok());
    }

    #[test]
    fn accepts_any_casing_of_the_bearer_scheme() {
        // RFC 7235: the auth-scheme is case-insensitive, and proxies rewrite it.
        for scheme in ["Bearer", "bearer", "BEARER", "BeArEr"] {
            let result =
                interceptor().call(request_with_authorization(&format!("{scheme} {TOKEN}")));
            assert!(result.is_ok(), "{scheme} should be accepted");
        }
    }

    #[test]
    fn rejects_another_scheme_carrying_the_right_token() {
        let status = interceptor()
            .call(request_with_authorization(&format!("Basic {TOKEN}")))
            .expect_err("only the Bearer scheme is accepted");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn rejects_a_missing_header() {
        let status = interceptor()
            .call(Request::new(()))
            .expect_err("no credentials must be refused");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn rejects_a_token_without_the_bearer_prefix() {
        let status = interceptor()
            .call(request_with_authorization(TOKEN))
            .expect_err("a bare token must be refused");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn rejects_a_wrong_token_of_equal_length() {
        let mut wrong = TOKEN.to_string();
        wrong.pop();
        wrong.push('0');
        assert_eq!(wrong.len(), TOKEN.len());

        let status = interceptor()
            .call(request_with_authorization(&format!("Bearer {wrong}")))
            .expect_err("a wrong token must be refused");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn rejects_a_token_that_is_a_prefix_of_the_real_one() {
        let status = interceptor()
            .call(request_with_authorization(&format!(
                "Bearer {}",
                &TOKEN[..TOKEN.len() - 1]
            )))
            .expect_err("a truncated token must be refused");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn credentials_match_only_on_exact_equality() {
        assert!(credentials_match("abc", "abc"));
        assert!(!credentials_match("abc", "abd"));
        assert!(!credentials_match("abc", "ab"));
        assert!(!credentials_match("ab", "abc"));
        // The credential itself stays case-sensitive even though the scheme
        // is not.
        assert!(!credentials_match("ABC", "abc"));
    }
}
