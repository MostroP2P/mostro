//! Shared helpers for the CF-3 mint-backed integration suite
//! (`docs/cashu/01-fundamentals.md` §6).
//!
//! Everything here is env-gated on `CASHU_TEST_MINT_URL`: when the variable
//! is unset (offline CI, local runs without the mint container) callers
//! skip instead of failing, so the default test suite never depends on
//! network access.
//!
//! Wallet-level helpers (fund a test wallet, build a 2-of-3 escrow token)
//! land here once the `cdk` dependency is on `main` (CF-2, PR #798) — the
//! feature tracks reuse them for their end-to-end lock/release tests.

use std::time::{Duration, Instant};

/// The test-mint URL, if the harness is active. Trailing slashes are
/// trimmed so callers can naively join paths.
pub fn mint_url_from_env() -> Option<String> {
    std::env::var("CASHU_TEST_MINT_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
}

/// Poll the mint's NUT-06 info endpoint until it answers or `timeout`
/// elapses (the container needs a few seconds to come up in CI). Returns
/// the parsed `/v1/info` JSON.
pub async fn wait_for_mint(mint_url: &str, timeout: Duration) -> Result<serde_json::Value, String> {
    let client = reqwest::Client::new();
    let info_url = format!("{mint_url}/v1/info");
    let deadline = Instant::now() + timeout;
    loop {
        match client.get(&info_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                return resp
                    .json::<serde_json::Value>()
                    .await
                    .map_err(|e| format!("mint info was not JSON: {e}"));
            }
            _ if Instant::now() >= deadline => {
                return Err(format!(
                    "mint at {info_url} not reachable within {timeout:?}"
                ));
            }
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
}
