//! CF-3 mint-backed integration suite (skeleton)
//! (`docs/cashu/01-fundamentals.md` §6).
//!
//! Runs against the throwaway nutshell mint from
//! `docker-compose.cashu.yml`, driven by `.github/workflows/cashu.yml`
//! (nightly + opt-in `cashu` PR label — never a default required check).
//! Env-gated: every test skips cleanly when `CASHU_TEST_MINT_URL` is
//! unset, so `cargo test` stays offline by default.

mod common;

/// CF-3 Definition of Done: the mint is reachable and returns its info.
/// Also sanity-checks the same escrow prerequisites
/// `CashuClient::connect` enforces (NUT-07 checkstate, NUT-11 P2PK,
/// NUT-12 DLEQ) so a mint-image bump that drops one fails loudly here
/// instead of inside a track's e2e test.
#[tokio::test]
async fn mint_is_reachable_and_supports_escrow_nuts() {
    let Some(mint_url) = common::mint_url_from_env() else {
        eprintln!("CASHU_TEST_MINT_URL unset; skipping mint-backed integration test");
        return;
    };

    let info = common::wait_for_mint(&mint_url, std::time::Duration::from_secs(90))
        .await
        .expect("test mint must come up within the timeout");

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
}
