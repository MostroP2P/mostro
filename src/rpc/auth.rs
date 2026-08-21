//! Bearer-token authentication for the admin gRPC surface.
//!
//! Every admin RPC is executed under the daemon's own Nostr identity (see
//! `crate::rpc::service`), and downstream authorization grants that identity
//! full privilege — `db::ensure_dispute_finalize_permission` short-circuits its
//! solver-category check for the daemon key. There is therefore no
//! message-level authorization left to fall back on: reaching the port is the
//! authorization, so the transport has to be the gate.
//!
//! The authentication *decision* is deliberately not rate-limited.
//! `MIN_RPC_TOKEN_LEN` keeps the search space far out of reach of online
//! guessing, and tonic's [`Interceptor`] is synchronous while
//! [`crate::rpc::rate_limiter::RateLimiter`] is async, so wiring one in would
//! mean a second, parallel limiter for no security gain.
//!
//! The *logging* is throttled, which is a different problem. The rate limiter
//! runs inside the handlers (`check_rate_limit` in [`crate::rpc::service`]), so
//! a request rejected here never reaches it. One `warn!` per rejected request
//! is harmless on loopback, but under `[rpc].allow_remote = true` it hands any
//! host that can reach the port a free log line per request — unbounded disk
//! growth, and enough noise to bury the genuine audit lines. So the first
//! rejection from a peer within [`LOG_WINDOW`] is logged at `warn!` and the rest
//! at `debug!`. The peer table that makes this possible is itself capped, or it
//! would just move the unbounded growth from the log to memory.

use secrecy::{ExposeSecret, SecretString};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tonic::service::Interceptor;
use tonic::{Request, Status};
use tracing::{debug, warn};

const AUTHORIZATION_HEADER: &str = "authorization";
/// RFC 7235 defines the auth-scheme as case-insensitive, and proxies do
/// normalize it, so the scheme is matched without regard to case. The
/// credential that follows is still compared byte for byte.
const BEARER_SCHEME: &str = "Bearer";
/// How long one rejected peer stays quiet after its `warn!` line. Short enough
/// that a real operator debugging a wrong token still sees a line per attempt
/// once they pause, long enough that a flood costs one line per minute.
const LOG_WINDOW: Duration = Duration::from_secs(60);
/// Ceiling on tracked peers. Without it a caller that spoofs a fresh source
/// address per request would grow this table forever, which is the same
/// resource exhaustion the throttle exists to prevent.
const MAX_TRACKED_PEERS: usize = 1024;

/// Rejects any request that does not carry the configured bearer token.
#[derive(Clone)]
pub struct BearerAuth {
    token: Arc<SecretString>,
    /// Last time each peer was logged at `warn!`. `None` keys the peers tonic
    /// could not attribute to an address, so they are throttled as one bucket
    /// rather than escaping the cap.
    ///
    /// A `std::sync::Mutex` because [`Interceptor::call`] is synchronous; the
    /// critical section is a hash lookup and never awaits. `Arc` because tonic
    /// clones the interceptor per connection and the table has to be shared.
    warned: Arc<Mutex<HashMap<Option<IpAddr>, Instant>>>,
}

impl BearerAuth {
    pub fn new(token: SecretString) -> Self {
        Self {
            token: Arc::new(token),
            warned: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// True when this rejection deserves a `warn!` rather than a `debug!`.
    ///
    /// `now` is a parameter so the window can be tested without sleeping.
    fn should_warn(&self, peer: Option<IpAddr>, now: Instant) -> bool {
        // A poisoned lock means another thread panicked mid-update. The table
        // is a logging heuristic, so recovering the map beats propagating a
        // panic out of an interceptor and killing the connection.
        let mut warned = self.warned.lock().unwrap_or_else(PoisonError::into_inner);

        if let Some(last) = warned.get(&peer) {
            if now.duration_since(*last) < LOG_WINDOW {
                return false;
            }
        } else if warned.len() >= MAX_TRACKED_PEERS {
            warned.retain(|_, last| now.duration_since(*last) < LOG_WINDOW);
            // Still full: every slot belongs to a peer inside its window, so
            // this one is part of a flood. Stay quiet rather than grow.
            if warned.len() >= MAX_TRACKED_PEERS {
                return false;
            }
        }

        warned.insert(peer, now);
        true
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
                // RFC 7235 allows `1*SP` between the scheme and the credential,
                // and token68 contains no spaces, so trimming the rest is
                // lossless.
                let credential = credential.trim_start_matches(' ');
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
                let peer = request.remote_addr().map(|addr| addr.ip());
                if self.should_warn(peer, Instant::now()) {
                    match peer {
                        Some(ip) => warn!("Rejected unauthenticated admin RPC from {}", ip),
                        None => warn!("Rejected unauthenticated admin RPC from an unknown peer"),
                    }
                } else {
                    match peer {
                        Some(ip) => debug!(
                            "Rejected unauthenticated admin RPC from {} (further rejections from \
                             this peer are logged at debug for up to {}s)",
                            ip,
                            LOG_WINDOW.as_secs()
                        ),
                        None => debug!("Rejected unauthenticated admin RPC from an unknown peer"),
                    }
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
    fn accepts_extra_spaces_after_the_scheme() {
        // RFC 7235 auth-param grammar is `scheme 1*SP token68`, and proxies do
        // rewrite the separator.
        let result = interceptor().call(request_with_authorization(&format!("Bearer   {TOKEN}")));
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

    fn peer(last_octet: u8) -> Option<IpAddr> {
        Some(IpAddr::from([192, 0, 2, last_octet]))
    }

    #[test]
    fn a_flooding_peer_is_warned_about_once_per_window() {
        let auth = interceptor();
        let start = Instant::now();

        assert!(auth.should_warn(peer(1), start));
        for attempt in 1..100 {
            assert!(
                !auth.should_warn(peer(1), start + Duration::from_millis(attempt)),
                "attempt {attempt} must not produce a second warn line"
            );
        }
        // Once the window closes the peer is audible again, so a slow retry
        // loop still leaves a trail.
        assert!(auth.should_warn(peer(1), start + LOG_WINDOW));
    }

    #[test]
    fn each_peer_gets_its_own_window() {
        let auth = interceptor();
        let now = Instant::now();
        assert!(auth.should_warn(peer(1), now));
        assert!(auth.should_warn(peer(2), now));
        assert!(auth.should_warn(None, now));
        // ...and the second attempt from each is throttled independently.
        assert!(!auth.should_warn(peer(1), now));
        assert!(!auth.should_warn(peer(2), now));
        assert!(!auth.should_warn(None, now));
    }

    #[test]
    fn the_peer_table_cannot_grow_without_bound() {
        // A caller spoofing a fresh source address per request must not be able
        // to turn the log throttle into a memory leak.
        let auth = interceptor();
        let now = Instant::now();
        for octet in 0..=u8::MAX {
            for third in 0..=u8::MAX {
                auth.should_warn(Some(IpAddr::from([192, 0, third, octet])), now);
            }
        }
        let tracked = auth
            .warned
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len();
        assert!(
            tracked <= MAX_TRACKED_PEERS,
            "{tracked} peers tracked, cap is {MAX_TRACKED_PEERS}"
        );
    }

    #[test]
    fn expired_entries_are_reclaimed_when_the_table_fills() {
        let auth = interceptor();
        let start = Instant::now();
        for third in 0..=u8::MAX {
            for fourth in 0..=u8::MAX {
                auth.should_warn(Some(IpAddr::from([198, 51, third, fourth])), start);
            }
        }
        // Every tracked peer is now outside its window, so a new peer is both
        // admitted and audible rather than silently dropped.
        assert!(auth.should_warn(peer(7), start + LOG_WINDOW));
    }
}
