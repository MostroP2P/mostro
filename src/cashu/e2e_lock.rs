//! Mint-backed end-to-end harness for the Track A **TA-1** escrow lock
//! (`docs/cashu/02-track-a-lock.md` §8, Definition of Done item 1).
//!
//! This is the manual-test rig: it mints a **real** 2-of-3 escrow token at a
//! live mint and drives `add_cashu_escrow_action` with it, which is the only
//! way to exercise steps 5–8 of the handler. The in-module unit tests cannot —
//! they stop before the mint round-trip.
//!
//! `#[ignore]`d and env-gated, exactly like `tests/cashu_mint.rs`: a plain
//! `cargo test` reports it as *ignored*, and even under `--ignored` it returns
//! early when `CASHU_TEST_MINT_URL` is unset.
//!
//! ```text
//! # Throwaway mint (no real funds — start here):
//! docker compose -f docker-compose.cashu.yml up -d
//! CASHU_TEST_MINT_URL=http://127.0.0.1:3338 \
//!   cargo test --bin mostrod cashu::e2e_lock -- --ignored --nocapture
//!
//! # Real mint (REAL SATS — see the safety notes below):
//! CASHU_TEST_MINT_URL=https://mint.cubabitcoin.org CASHU_E2E_AMOUNT=4 \
//!   cargo test --bin mostrod cashu::e2e_lock -- --ignored --nocapture
//! ```
//!
//! **Safety on a real mint.** The escrow token is locked to a 2-of-3 over three
//! freshly generated keys, so the sats are recoverable *only* with those keys.
//! The harness therefore writes them to `CASHU_E2E_KEYS_FILE` (default
//! `./cashu-e2e-keys.json`) **before** anything is minted, and configures the
//! locktime floor to `CASHU_E2E_LOCKTIME_DAYS` (default 1) so the
//! seller-recovery path (`refund = [P_S]`) opens a day later instead of the
//! 15-day production default. Keep the amount small.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use cdk::amount::{FeeAndAmounts, SplitTarget};
use cdk::mint_url::MintUrl;
use cdk::nuts::{
    Conditions, CurrencyUnit, MintQuoteBolt11Request, MintQuoteState, MintRequest, PaymentMethod,
    PreMintSecrets, SigFlag, SpendingConditions, Token,
};
use cdk::wallet::MintConnector;
use cdk::{Amount, HttpClient};

use crate::app::add_cashu_escrow::add_cashu_escrow_action;
use crate::app::context::test_utils::{test_settings, TestContextBuilder};
use crate::app::context::AppContext;
use crate::cashu::{cashu_pubkey_from_xonly_hex, CashuClient};
use crate::config::types::CashuSettings;
use chrono::Utc;
use mostro_core::db::Crud;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::SqlitePool;

/// Seconds in a day — the locktime floor is configured in days.
const SECONDS_PER_DAY: u64 = 86_400;

/// How long to wait for a human to pay the mint quote on a real mint. The
/// throwaway FakeWallet mint settles instantly and never uses this.
const INVOICE_WAIT: Duration = Duration::from_secs(300);

fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn amount_sats() -> u64 {
    env_var("CASHU_E2E_AMOUNT")
        .and_then(|v| v.parse().ok())
        .unwrap_or(4)
}

fn locktime_days() -> u64 {
    env_var("CASHU_E2E_LOCKTIME_DAYS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

/// The three parties' keys. Generated fresh per run and persisted before any
/// sats move, because they are the only way to redeem the escrow afterwards.
struct EscrowKeys {
    buyer: Keys,
    seller: Keys,
    mostro: Keys,
}

impl EscrowKeys {
    fn generate() -> Self {
        Self {
            buyer: Keys::generate(),
            seller: Keys::generate(),
            mostro: Keys::generate(),
        }
    }

    /// Write the keys out **before** minting. On a real mint these are the
    /// difference between "recoverable escrow" and "burnt sats".
    fn persist(&self) -> String {
        let path =
            env_var("CASHU_E2E_KEYS_FILE").unwrap_or_else(|| "./cashu-e2e-keys.json".to_string());
        let json = format!(
            r#"{{
  "note": "Track A TA-1 e2e harness. These keys redeem the escrow token — do not delete until the sats are swept.",
  "buyer_trade_key":  {{ "secret": "{}", "pubkey": "{}" }},
  "seller_trade_key": {{ "secret": "{}", "pubkey": "{}" }},
  "mostro_key":       {{ "secret": "{}", "pubkey": "{}" }}
}}
"#,
            self.buyer.secret_key().to_secret_hex(),
            self.buyer.public_key(),
            self.seller.secret_key().to_secret_hex(),
            self.seller.public_key(),
            self.mostro.secret_key().to_secret_hex(),
            self.mostro.public_key(),
        );
        std::fs::write(&path, json)
            .expect("must be able to persist the escrow keys before minting");
        path
    }
}

/// Mint a token locked to the §4B escrow shape: `data = P_S`,
/// `pubkeys = [P_B, P_M]`, `n_sigs = 2`, `locktime` present, `refund = [P_S]`.
///
/// Minting straight into the P2PK condition (rather than minting plain ecash
/// and swapping it) keeps the token value exact on a mint that charges an
/// input fee — the cubabitcoin keyset charges 100 ppk, and a swap would eat
/// into the escrow amount the handler checks for equality.
async fn mint_escrow_token(
    mint_url: &str,
    keys: &EscrowKeys,
    amount: u64,
    locktime: u64,
) -> String {
    let url = MintUrl::from_str(mint_url).expect("mint url must parse");
    let client = HttpClient::new(url.clone(), None);

    let p_b = cashu_pubkey_from_xonly_hex(&keys.buyer.public_key().to_string()).unwrap();
    let p_s = cashu_pubkey_from_xonly_hex(&keys.seller.public_key().to_string()).unwrap();
    let p_m = cashu_pubkey_from_xonly_hex(&keys.mostro.public_key().to_string()).unwrap();

    let conditions = SpendingConditions::P2PKConditions {
        data: p_s,
        conditions: Some(Conditions {
            locktime: Some(locktime),
            pubkeys: Some(vec![p_b, p_m]),
            refund_keys: Some(vec![p_s]),
            num_sigs: Some(2),
            sig_flag: SigFlag::SigInputs,
            num_sigs_refund: None,
        }),
    };

    // The active sat keyset — the same one `CashuClient::connect` insists on.
    let keysets = client
        .get_mint_keysets()
        .await
        .expect("mint must serve /v1/keysets");
    let keyset = keysets
        .keysets
        .iter()
        .find(|k| k.active && k.unit == CurrencyUnit::Sat)
        .expect("mint must have an active sat keyset");
    let keyset_id = keyset.id;

    // 1. Ask for an invoice.
    let quote = client
        .post_mint_quote(
            MintQuoteBolt11Request {
                amount: Amount::from(amount),
                unit: CurrencyUnit::Sat,
                description: Some("mostro TA-1 escrow e2e".to_string()),
                pubkey: None,
            }
            .into(),
        )
        .await
        .expect("mint must issue a quote");

    println!("\n  Pay this invoice to fund the escrow ({amount} sat):\n");
    println!("  {}\n", quote.request());

    // 2. Wait for payment. FakeWallet settles instantly; a real mint waits for
    //    a human.
    let deadline = std::time::Instant::now() + INVOICE_WAIT;
    loop {
        let status = client
            .get_mint_quote_status(PaymentMethod::BOLT11, quote.quote())
            .await
            .expect("mint must report quote status");
        if status.state() == Some(MintQuoteState::Paid) {
            println!("  invoice paid ✓");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "invoice was not paid within {INVOICE_WAIT:?}"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // 3. Mint directly into the escrow condition. The allowed denominations
    //    come from the keyset itself — an empty list means "nothing to split
    //    into" and the amount cannot be represented.
    let keyset_keys = client
        .get_mint_keyset(keyset_id)
        .await
        .expect("mint must serve the keyset keys");
    // `KeySet.keys` is a wrapper; `.keys()` unwraps it to the amount->pubkey
    // map, whose keys are the denominations the mint will sign.
    let mut denominations: Vec<u64> = keyset_keys
        .keys
        .keys()
        .keys()
        .map(|amount| u64::from(*amount))
        .collect();
    denominations.sort_unstable();

    let premint = PreMintSecrets::with_conditions(
        keyset_id,
        Amount::from(amount),
        &SplitTarget::default(),
        &conditions,
        &FeeAndAmounts::from((keyset.input_fee_ppk, denominations)),
    )
    .expect("premint secrets must build");

    let response = client
        .post_mint(
            &PaymentMethod::BOLT11,
            MintRequest {
                quote: quote.quote().to_string(),
                outputs: premint.blinded_messages(),
                signature: None,
            },
        )
        .await
        .expect("mint must issue the blinded signatures");

    let proofs = cdk::dhke::construct_proofs(
        response.signatures,
        premint.rs(),
        premint.secrets(),
        &keyset_keys.keys,
    )
    .expect("proofs must reconstruct");

    Token::new(url, proofs, None, CurrencyUnit::Sat).to_string()
}

async fn temp_pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::migrate!().run(&pool).await.unwrap();
    pool
}

/// A taken Cashu order parked in `WaitingPayment` — the state TA-2 will
/// produce, seeded here by hand because `dispatch_cashu` still rejects
/// `NewOrder`/`TakeSell` until TA-2 lands.
async fn seed_waiting_payment_order(pool: &SqlitePool, keys: &EscrowKeys, amount: u64) -> Order {
    Order {
        id: uuid::Uuid::new_v4(),
        status: Status::WaitingPayment.to_string(),
        kind: mostro_core::order::Kind::Sell.to_string(),
        fiat_code: "USD".to_string(),
        creator_pubkey: keys.seller.public_key().to_string(),
        seller_pubkey: Some(keys.seller.public_key().to_string()),
        master_seller_pubkey: Some(keys.seller.public_key().to_string()),
        buyer_pubkey: Some(keys.buyer.public_key().to_string()),
        master_buyer_pubkey: Some(keys.buyer.public_key().to_string()),
        amount: amount as i64,
        fee: 0,
        fiat_amount: 10,
        ..Default::default()
    }
    .create(pool)
    .await
    .unwrap()
}

fn lock_message(order_id: uuid::Uuid, token: &str, mint_url: &str, keys: &EscrowKeys) -> Message {
    Message::new_order(
        Some(order_id),
        Some(1),
        None,
        Action::AddCashuEscrow,
        Some(Payload::CashuLockProof(CashuLockProof::new(
            token.to_string(),
            mint_url.to_string(),
            keys.buyer.public_key().to_string(),
            keys.seller.public_key().to_string(),
            keys.mostro.public_key().to_string(),
        ))),
    )
}

fn seller_event(keys: &EscrowKeys) -> UnwrappedMessage {
    UnwrappedMessage {
        message: Message::new_order(None, Some(1), None, Action::AddCashuEscrow, None),
        signature: None,
        sender: keys.seller.public_key(),
        identity: keys.seller.public_key(),
        created_at: Timestamp::now(),
    }
}

async fn build_ctx(pool: &SqlitePool, mint_url: &str) -> AppContext {
    let client = CashuClient::connect(mint_url)
        .await
        .expect("the mint must satisfy NUT-07/11/12 and expose an active sat keyset");
    TestContextBuilder::new()
        .with_pool(Arc::new(pool.clone()))
        .with_settings(test_settings())
        .build()
        .with_cashu_client(Arc::new(client))
}

/// The full TA-1 happy path against a live mint, followed by the two
/// review-driven guards this PR added.
#[tokio::test]
#[ignore = "requires a live mint and CASHU_TEST_MINT_URL; see the module docs"]
async fn escrow_lock_end_to_end_against_a_live_mint() {
    let Some(mint_url) = env_var("CASHU_TEST_MINT_URL") else {
        eprintln!("CASHU_TEST_MINT_URL unset; skipping the mint-backed e2e harness");
        return;
    };
    let amount = amount_sats();
    let days = locktime_days();

    // Settings first: the handler reads the configured mint and the locktime
    // floor out of the process-global config.
    let mut settings = test_settings();
    settings.cashu = Some(CashuSettings {
        enabled: true,
        mint_url: mint_url.clone(),
        escrow_locktime_days: days as u32,
    });
    let _ = crate::config::settings::init_mostro_settings(settings);

    let keys = EscrowKeys::generate();
    let keys_path = keys.persist();
    println!("\n=== TA-1 escrow lock e2e ===");
    println!("  mint:      {mint_url}");
    println!("  amount:    {amount} sat");
    println!("  keys file: {keys_path}  <- these redeem the escrow, keep them");

    // A locktime comfortably above the floor the handler enforces.
    let locktime = (Utc::now().timestamp() as u64) + (days + 1) * SECONDS_PER_DAY;
    let token = mint_escrow_token(&mint_url, &keys, amount, locktime).await;
    println!("  escrow token minted ✓");

    let pool = temp_pool().await;
    let ctx = build_ctx(&pool, &mint_url).await;
    let order = seed_waiting_payment_order(&pool, &keys, amount).await;
    println!("  order {} seeded in WaitingPayment", order.id);

    // --- Happy path -------------------------------------------------------
    let event = seller_event(&keys);
    let msg = lock_message(order.id, &token, &mint_url, &keys);
    add_cashu_escrow_action(&ctx, msg, &event, &keys.mostro)
        .await
        .expect("a valid escrow token must lock the order");

    let locked = Order::by_id(&pool, order.id).await.unwrap().unwrap();
    assert_eq!(locked.status, Status::Active.to_string());
    assert_eq!(locked.cashu_escrow_token.as_deref(), Some(token.as_str()));
    assert!(locked.cashu_escrow_locked_at.is_some());
    println!("  ✓ happy path: WaitingPayment -> Active, escrow persisted");

    // --- Replay recovery (step 3b, this PR) -------------------------------
    let msg = lock_message(order.id, &token, &mint_url, &keys);
    add_cashu_escrow_action(&ctx, msg, &seller_event(&keys), &keys.mostro)
        .await
        .expect("replaying our own lock must be a no-op, not a rejection");
    let after = Order::by_id(&pool, order.id).await.unwrap().unwrap();
    assert_eq!(
        after.cashu_escrow_locked_at, locked.cashu_escrow_locked_at,
        "a replay must not rewrite the lock"
    );
    println!("  ✓ replay re-notifies without rewriting state");

    // --- Cross-order reuse (step 6b, this PR) -----------------------------
    let second = seed_waiting_payment_order(&pool, &keys, amount).await;
    let msg = lock_message(second.id, &token, &mint_url, &keys);
    let reused = add_cashu_escrow_action(&ctx, msg, &seller_event(&keys), &keys.mostro).await;
    assert!(
        matches!(reused, Err(MostroCantDo(CantDoReason::InvalidCashuToken))),
        "the same token must not escrow a second order, got {reused:?}"
    );
    let untouched = Order::by_id(&pool, second.id).await.unwrap().unwrap();
    assert_eq!(untouched.status, Status::WaitingPayment.to_string());
    assert!(untouched.cashu_escrow_token.is_none());
    println!("  ✓ the same token cannot escrow a second order");

    println!("\n  All checks passed. The escrow token is live at the mint:");
    println!("  {token}\n");
    println!("  Sweep it with the keys in {keys_path} (any 2 of the 3 sign),");
    println!("  or wait out the locktime and reclaim with the seller key alone.\n");
}
