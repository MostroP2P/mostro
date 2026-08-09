//! CF-3 mint-backed integration suite (skeleton)
//! (`docs/cashu/01-fundamentals.md` §6).
//!
//! Runs against the throwaway nutshell mint from
//! `docker-compose.cashu.yml`, driven by `.github/workflows/cashu.yml`
//! (nightly + opt-in `cashu` PR label — never a default required check).
//! `#[ignore]`d and env-gated: a plain `cargo test` reports it as *ignored*,
//! and even under `--ignored` it returns early when `CASHU_TEST_MINT_URL` is
//! unset, so the suite stays offline by default.

mod common;

/// CF-3 Definition of Done: the mint is reachable and meets **every**
/// prerequisite `CashuClient::connect` enforces (`src/cashu/mod.rs`), so a
/// mint-image bump that drops one fails loudly here instead of inside a
/// track's e2e test. Those are: NUT-07 (token state check), NUT-11 (P2PK),
/// NUT-12 (DLEQ), **and an active `sat` keyset** — `connect` rejects a mint
/// with `"no active sat keyset"` (M-3: every downstream amount check is in
/// sats), so the harness has to assert it too or it would pass while
/// `connect` fails.
///
/// `#[ignore]`d so a plain `cargo test` skips it as *ignored* rather than
/// reporting a hollow `1 passed`. CI runs it with `--ignored` and sets
/// `CASHU_REQUIRE_MINT`, which turns a missing URL into a hard failure (see
/// `common::mint_url_from_env`).
#[tokio::test]
#[ignore = "requires a running mint and CASHU_TEST_MINT_URL; run via cashu.yml or `--ignored`"]
async fn mint_is_reachable_and_supports_escrow_nuts() {
    let Some(mint_url) = common::mint_url_from_env() else {
        eprintln!("CASHU_TEST_MINT_URL unset; skipping mint-backed integration test");
        return;
    };

    let info = common::wait_for_mint(&mint_url, std::time::Duration::from_secs(90))
        .await
        .expect("test mint must come up within the timeout");

    // NUT-07 / NUT-11 / NUT-12 support, from /v1/info.
    let nuts = info.get("nuts").cloned().unwrap_or_default();
    for nut in ["7", "11", "12"] {
        let supported = nuts
            .get(nut)
            .and_then(|n| n.get("supported"))
            .and_then(|v| v.as_bool());
        assert_eq!(
            supported,
            Some(true),
            "test mint must support NUT-{nut} (escrow prerequisite); mint info: {info}"
        );
    }

    // Active `sat` keyset, from /v1/keysets (NUT-02). This mirrors the
    // `ks.active && ks.unit == CurrencyUnit::Sat` check in
    // `CashuClient::connect`; without it the harness would go green against a
    // mint serving only usd/msat, on which `connect` returns
    // "Mint has no active sat keyset".
    let keysets = common::mint_get_json(&mint_url, "/v1/keysets")
        .await
        .expect("mint must serve /v1/keysets");
    let has_active_sat = keysets
        .get("keysets")
        .and_then(|k| k.as_array())
        .map(|arr| {
            arr.iter().any(|ks| {
                ks.get("unit").and_then(|u| u.as_str()) == Some("sat")
                    // NUT-02: `active` defaults to true when absent.
                    && ks.get("active").and_then(|a| a.as_bool()).unwrap_or(true)
            })
        })
        .unwrap_or(false);
    assert!(
        has_active_sat,
        "test mint must expose an active `sat` keyset (escrow prerequisite); keysets: {keysets}"
    );
}
