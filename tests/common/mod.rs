//! Shared helpers for Cashu integration tests (F6 harness).
//!
//! These helpers gate every Cashu test behind a running test mint, discovered
//! via the `CASHU_TEST_MINT_URL` env var. When the var is unset the tests skip
//! cleanly, so the regular `cargo test` job (which never starts a mint) stays
//! green while the dedicated `cashu-mint` CI job exercises them for real.
#![allow(dead_code)] // not every test binary uses every helper

use std::env;

/// Env var pointing integration tests at a running Cashu test mint.
pub const MINT_URL_ENV: &str = "CASHU_TEST_MINT_URL";

/// Returns the configured test mint URL, or `None` when the env var is unset
/// or empty.
pub fn test_mint_url() -> Option<String> {
    env::var(MINT_URL_ENV).ok().filter(|s| !s.is_empty())
}

/// Resolves the test mint URL or logs a skip notice and returns `None`.
///
/// Idiomatic use at the top of a test:
/// ```ignore
/// let Some(mint_url) = common::require_mint() else { return };
/// ```
pub fn require_mint() -> Option<String> {
    match test_mint_url() {
        Some(url) => Some(url.trim_end_matches('/').to_string()),
        None => {
            eprintln!(
                "skipping Cashu integration test: {MINT_URL_ENV} not set \
                 (start a mint with `docker compose -f docker-compose.cashu.yml up -d`)"
            );
            None
        }
    }
}

// TODO(F4): 2-of-3 locked-token fixtures live here once the `cdk` dependency
// lands with the CashuClient library. The tracks (A/D) reuse them to build
// NUT-10/NUT-11 P2PK tokens bound to per-order trade pubkeys. Until then this
// module only exposes mint discovery, since `src/cashu/` does not exist yet.
