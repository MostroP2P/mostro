//! Env-gated smoke test for the Cashu test mint (F6 harness).
//!
//! Skips cleanly unless `CASHU_TEST_MINT_URL` points at a running mint — the
//! dedicated `cashu-mint` CI job, or a local `docker-compose.cashu.yml`.

mod common;

/// The mint answers NUT-06 `/v1/info` with a JSON document advertising the
/// NUTs it supports. This is the minimal liveness check the feature tracks
/// build on before exercising token locking and redemption.
#[tokio::test]
async fn mint_info_reachable() {
    let Some(mint_url) = common::require_mint() else {
        return;
    };

    let url = format!("{mint_url}/v1/info");
    let resp = reqwest::get(&url)
        .await
        .expect("request to mint /v1/info failed");
    assert!(
        resp.status().is_success(),
        "mint /v1/info returned {}",
        resp.status()
    );

    let info: serde_json::Value = resp.json().await.expect("mint /v1/info is not JSON");
    assert!(
        info.get("nuts").is_some(),
        "mint info missing `nuts` field: {info}"
    );
}
