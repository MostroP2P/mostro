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
use tonic::service::Interceptor;
use tonic::{Request, Status};
use tracing::warn;

const AUTHORIZATION_HEADER: &str = "authorization";
const BEARER_PREFIX: &str = "Bearer ";

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
            .and_then(|value| value.strip_prefix(BEARER_PREFIX));

        match presented {
            Some(candidate)
                if constant_time_eq(
                    candidate.as_bytes(),
                    self.token.expose_secret().as_bytes(),
                ) =>
            {
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

/// Compare two byte strings without leaking how far they matched.
///
/// Length is not a secret here (the token length is fixed by the operator's
/// config), but the contents are, so the loop always runs to the end.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in a.iter().zip(b.iter()) {
        difference |= left ^ right;
    }
    difference == 0
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
    fn constant_time_eq_matches_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
