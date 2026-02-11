//! Simple in-memory rate limiter for RPC endpoints.
//!
//! Tracks failed attempts per client IP with exponential backoff and lockout.
//! Designed for the `ValidateDbPassword` endpoint to prevent brute-force attacks.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::warn;

/// Configuration for the rate limiter.
const MAX_ATTEMPTS: u32 = 5;
const LOCKOUT_DURATION: Duration = Duration::from_secs(300); // 5 minutes
const BASE_DELAY_MS: u64 = 1000; // 1 second base delay
const DEFAULT_RETENTION: Duration = Duration::from_secs(3600); // 1 hour

/// Tracks the state of failed attempts for a single client.
#[derive(Debug)]
struct ClientState {
    /// Number of consecutive failed attempts.
    failed_attempts: u32,
    /// Timestamp of the last failed attempt.
    last_attempt: Instant,
    /// Whether the client is currently locked out.
    locked_until: Option<Instant>,
}

impl ClientState {
    fn new() -> Self {
        Self {
            failed_attempts: 0,
            last_attempt: Instant::now(),
            locked_until: None,
        }
    }
}

/// In-memory rate limiter keyed by client IP address.
pub struct RateLimiter {
    clients: Mutex<HashMap<String, ClientState>>,
    retention: Duration,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(DEFAULT_RETENTION)
    }
}

impl RateLimiter {
    pub fn new(retention: Duration) -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            retention,
        }
    }

    /// Remove entries whose `last_attempt` is older than `self.retention`
    /// and whose lockout (if any) has already expired.
    fn evict_stale(&self, clients: &mut HashMap<String, ClientState>) {
        let now = Instant::now();
        let retention = self.retention;
        clients.retain(|_, state| {
            let within_retention = now.duration_since(state.last_attempt) <= retention;
            let still_locked = state
                .locked_until
                .is_some_and(|locked_until| now < locked_until);
            within_retention || still_locked
        });
    }

    /// Check if the client is allowed to make a request.
    /// Returns `Ok(())` if allowed, or `Err(message)` with a human-readable
    /// reason if the client is rate-limited or locked out.
    pub async fn check_rate_limit(&self, addr: &SocketAddr) -> Result<(), String> {
        let key = addr.ip().to_string();
        let mut clients = self.clients.lock().await;

        self.evict_stale(&mut clients);

        let state = clients.entry(key.clone()).or_insert_with(ClientState::new);

        // Check if client is locked out
        if let Some(locked_until) = state.locked_until {
            if Instant::now() < locked_until {
                let remaining = locked_until.duration_since(Instant::now());
                warn!(
                    "Rate limit: client {} is locked out for {} more seconds",
                    key,
                    remaining.as_secs()
                );
                return Err(format!(
                    "Too many failed attempts. Locked out for {} seconds.",
                    remaining.as_secs()
                ));
            }
            // Lockout expired, reset state
            state.failed_attempts = 0;
            state.locked_until = None;
        }

        Ok(())
    }

    /// Record a failed attempt for the client.
    /// Applies exponential backoff delay and lockout after MAX_ATTEMPTS.
    pub async fn record_failure(&self, addr: &SocketAddr) {
        let key = addr.ip().to_string();
        let mut clients = self.clients.lock().await;

        self.evict_stale(&mut clients);

        let state = clients.entry(key.clone()).or_insert_with(ClientState::new);
        state.failed_attempts += 1;
        state.last_attempt = Instant::now();

        if state.failed_attempts >= MAX_ATTEMPTS {
            state.locked_until = Some(Instant::now() + LOCKOUT_DURATION);
            warn!(
                "Rate limit: client {} locked out for {} seconds after {} failed attempts",
                key,
                LOCKOUT_DURATION.as_secs(),
                state.failed_attempts
            );
        } else {
            // Apply exponential backoff delay
            let delay_ms = BASE_DELAY_MS * 2u64.pow(state.failed_attempts.saturating_sub(1));
            warn!(
                "Rate limit: client {} failed attempt #{}, applying {}ms delay",
                key, state.failed_attempts, delay_ms
            );
            // Release the lock before sleeping
            drop(clients);
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }

    /// Record a successful attempt, resetting the client's failure state.
    pub async fn record_success(&self, addr: &SocketAddr) {
        let key = addr.ip().to_string();
        let mut clients = self.clients.lock().await;

        self.evict_stale(&mut clients);

        clients.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn test_addr(last_octet: u8) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, last_octet)), 50051)
    }

    #[tokio::test]
    async fn test_first_attempt_allowed() {
        let limiter = RateLimiter::new(DEFAULT_RETENTION);
        assert!(limiter.check_rate_limit(&test_addr(1)).await.is_ok());
    }

    #[tokio::test]
    async fn test_lockout_after_max_attempts() {
        let limiter = RateLimiter::new(DEFAULT_RETENTION);
        let addr = test_addr(2);

        for _ in 0..MAX_ATTEMPTS {
            limiter.record_failure(&addr).await;
        }

        assert!(limiter.check_rate_limit(&addr).await.is_err());
    }

    #[tokio::test]
    async fn test_success_resets_state() {
        let limiter = RateLimiter::new(DEFAULT_RETENTION);
        let addr = test_addr(3);

        limiter.record_failure(&addr).await;
        limiter.record_failure(&addr).await;
        limiter.record_success(&addr).await;

        // Should be allowed again
        assert!(limiter.check_rate_limit(&addr).await.is_ok());
    }

    #[tokio::test]
    async fn test_different_ips_independent() {
        let limiter = RateLimiter::new(DEFAULT_RETENTION);
        let addr1 = test_addr(4);
        let addr2 = test_addr(5);

        for _ in 0..MAX_ATTEMPTS {
            limiter.record_failure(&addr1).await;
        }

        // addr1 locked out, addr2 should be fine
        assert!(limiter.check_rate_limit(&addr1).await.is_err());
        assert!(limiter.check_rate_limit(&addr2).await.is_ok());
    }

    #[tokio::test]
    async fn test_stale_entries_evicted() {
        // Use zero retention so entries become stale immediately
        let limiter = RateLimiter::new(Duration::ZERO);
        let addr = test_addr(6);

        // Record a failure — this inserts an entry
        limiter.record_failure(&addr).await;

        // The entry's last_attempt is now in the past (or at best equal to now)
        // and there is no active lockout (only 1 failure < MAX_ATTEMPTS).
        // With ZERO retention the entry is stale, so the next call should evict it.
        assert!(limiter.check_rate_limit(&addr).await.is_ok());

        // Verify the map is empty (stale entry was evicted, and the fresh
        // entry inserted by check_rate_limit has zero failures so it's benign,
        // but let's verify the old failure state is gone).
        let clients = limiter.clients.lock().await;
        // The entry exists (created by check_rate_limit) but has 0 failures
        let state = clients.get(&addr.ip().to_string());
        assert!(state.is_none() || state.unwrap().failed_attempts == 0);
    }
}
