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

/// Set by CI to turn a missing mint URL from "skip" into a hard failure.
const REQUIRE_MINT_VAR: &str = "CASHU_REQUIRE_MINT";

/// The test-mint URL, if the harness is active. Trailing slashes are
/// trimmed so callers can naively join paths.
///
/// Returns `None` when `CASHU_TEST_MINT_URL` is unset so a plain local
/// `cargo test` stays offline — but panics instead when `CASHU_REQUIRE_MINT`
/// is set, which `.github/workflows/cashu.yml` does.
///
/// The distinction matters because skipping is indistinguishable from
/// succeeding in libtest output: a skipped mint test still reports
/// `1 passed`, not `ignored`. Without this guard a renamed variable, a typo
/// or a dropped `env:` block would leave the Cashu job green while it
/// exercised nothing at all — the one failure mode a mint harness must not
/// have.
pub fn mint_url_from_env() -> Option<String> {
    let url = std::env::var("CASHU_TEST_MINT_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty());

    assert!(
        !(url.is_none() && std::env::var_os(REQUIRE_MINT_VAR).is_some()),
        "{REQUIRE_MINT_VAR} is set but CASHU_TEST_MINT_URL is empty or missing: \
         the mint-backed suite would have silently reported success without \
         testing anything. Check the `env:` block in \
         .github/workflows/cashu.yml."
    );

    url
}

/// Per-request cap. A bare `reqwest::Client` has no default timeout, so a
/// mint that accepts the TCP connection but never answers `/v1/info` would
/// hang `send().await` forever — the `wait_for_mint` deadline is only
/// re-checked *between* requests, so it would never fire, and the job would
/// run until GitHub's global timeout. This bounds every individual probe.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Client with a per-request timeout, so a hung endpoint surfaces as a
/// retryable error instead of an unbounded await.
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("reqwest client with a timeout must build")
}

/// GET `{mint_url}{path}` once and parse the body as JSON. Each call is
/// bounded by [`REQUEST_TIMEOUT`].
pub async fn mint_get_json(mint_url: &str, path: &str) -> Result<serde_json::Value, String> {
    let url = format!("{mint_url}{path}");
    let resp = http_client()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("GET {url} failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET {url} returned {}", resp.status()));
    }
    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| format!("{url} body was not JSON: {e}"))
}

/// Poll the mint's NUT-06 info endpoint until it answers or `timeout`
/// elapses (the container needs a few seconds to come up in CI). Returns
/// the parsed `/v1/info` JSON. Each probe is bounded by [`REQUEST_TIMEOUT`],
/// so a hung endpoint cannot stall the loop past the deadline.
pub async fn wait_for_mint(mint_url: &str, timeout: Duration) -> Result<serde_json::Value, String> {
    let client = http_client();
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
