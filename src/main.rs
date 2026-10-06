pub mod app;
mod bitcoin_price;
pub mod cashu;
pub mod cli;
pub mod config;
pub mod db;
pub mod escrow;
pub mod flow;
pub mod lightning;
pub mod lnurl;
pub mod messages;
pub mod nip33;
pub mod price;
pub mod publish;
pub mod rpc;
pub mod scheduler;
pub mod spam_gate;
pub mod util;

/// Convenience alias mirroring the one `nostr` (pre-0.45) used to re-export
/// via `nostr_sdk::prelude::*`; nostr 0.45 dropped it, so it's recreated here
/// for the crate root and pulled into other modules via `use crate::Result;`.
pub type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

use crate::app::context::AppContext;
use crate::app::dev_fee::{dev_fee_payments_enabled, release_all_pending_claims};
use crate::app::maintenance::{node_identity_guard, MaintenanceState, NodeIdentityDecision};
use crate::app::serbero::{serbero_guard, SerberoDecision};
use crate::app::{run, run_cashu};
use crate::cli::settings_init;
use crate::config::{
    get_db_pool, Settings, DB_POOL, LN_STATUS, MESSAGE_QUEUES, MOSTRO_CONFIG, NOSTR_CLIENT,
};
use crate::db::find_held_invoices;
use crate::lightning::LnStatus;
use crate::lightning::LndConnector;
use crate::rpc::RpcServer;
use nostr_sdk::prelude::*;
use scheduler::start_scheduler;
use std::env;
use std::process::exit;
use std::sync::Arc;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use util::{get_nostr_client, invoice_subscribe};

#[tokio::main]
async fn main() -> Result<()> {
    // Clear screen
    clearscreen::clear().expect("Failed to clear screen");

    if cfg!(debug_assertions) {
        // Debug, show all error + mostro logs
        env::set_var("RUST_LOG", "error,mostro=info");
    } else {
        // Release, show only mostro logs
        env::set_var("RUST_LOG", "none,mostro=info");
    }

    // Tracing using RUST_LOG
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    // Init MOSTRO_SETTINGS oncelock with all settings variables from TOML file.
    // Print a bad configuration with `Display`, not `Debug`, so an operator
    // reads the reason (e.g. a leftover `transport = "gift-wrap"`) as text
    // instead of an escaped, single-line error value.
    if let Err(e) = settings_init() {
        eprintln!("Could not load settings: {e}");
        exit(1);
    }

    // Build and install the multi-source price manager (spec §9 Phase 1).
    // Done immediately after settings load so every later subsystem
    // (scheduler, util::get_bitcoin_price, RPC) can read prices through it.
    install_price_manager()?;

    // Connect to database
    if DB_POOL.set(db::connect().await?).is_err() {
        tracing::error!("No connection to database - closing Mostro!");
        exit(1);
    };

    // Payer history (docs/PAYER_HISTORY_ANTI_TRIANGULATION.md §10.7, §13):
    // re-evaluate the stored snapshots when the operator changed the D-7
    // thresholds. No-op unless the feature is enabled. A failure rolls the
    // pass back and is retried at the next boot.
    if let Some(warning) = app::payer::boot::cashu_conflict_warning(
        Settings::is_cashu_enabled(),
        Settings::is_payer_history_enabled(),
    ) {
        tracing::warn!("{warning}");
    }
    app::payer::boot::sync_experience_policy(
        get_db_pool().as_ref(),
        util::get_keys()?,
        Settings::get_payer_history(),
        Timestamp::now().as_secs() as i64,
    )
    .await?;

    // Serbero, the dispute assistant: the configured key must be a read-only
    // solver. Registered here when it has no row yet; any other kind of row
    // is left untouched and stops the boot (docs/SOLVER_PERMISSION_LEVELS.md).
    if let Some(serbero) = Settings::get_mostro().serbero_pubkey() {
        let node = util::get_keys()?.public_key();
        let npub = serbero.to_bech32().unwrap_or_else(|_| serbero.to_hex());
        let decision = serbero_guard(get_db_pool().as_ref(), &serbero, &node).await?;
        match decision {
            SerberoDecision::Registered => {
                tracing::info!("Serbero {npub} configured: registered as a read-only solver");
            }
            SerberoDecision::ReadOnlySolver => {
                tracing::info!("Serbero {npub} configured: read-only solver");
            }
            SerberoDecision::WriteSolver { category } => {
                tracing::error!(
                    "REFUSING TO START: serbero_pubkey {npub} is a solver with write permission \
                     (category {category}). Serbero must be read-only: give it its own key, or \
                     remove serbero_pubkey."
                );
            }
            SerberoDecision::NotASolver => {
                tracing::error!(
                    "REFUSING TO START: serbero_pubkey {npub} belongs to a user that is not a \
                     solver. Give Serbero its own key, or remove serbero_pubkey."
                );
            }
            SerberoDecision::NodeKey => {
                tracing::error!(
                    "REFUSING TO START: serbero_pubkey {npub} is this node's own key. Give \
                     Serbero its own key, or remove serbero_pubkey."
                );
            }
        }
        if !decision.allows_start() {
            exit(1);
        }
    }

    // Connect to relays
    if NOSTR_CLIENT.set(util::connect_nostr().await?).is_err() {
        tracing::error!("No connection to nostr relay - closing Mostro!");
        exit(1);
    };

    // Get mostro keys
    let mostro_keys = util::get_keys()?;

    // Subscribe only to the configured transport's kind. The only transport
    // is protocol v2 (NIP-44 direct, kind 14); it is still read from the
    // settings so kind, envelope and advertised version share one source.
    // See docs/TRANSPORT_V2_SPEC.md §8.
    let transport = Settings::get_mostro().transport;
    tracing::info!(
        "Transport: {} (protocol v{}, event kind {})",
        transport,
        transport.protocol_version(),
        transport.event_kind().as_u16()
    );
    let subscription = Filter::new()
        .pubkey(mostro_keys.public_key())
        .kind(transport.event_kind())
        .limit(0);

    let client = match get_nostr_client() {
        Ok(client) => client,
        Err(e) => {
            tracing::error!("Failed to initialize Nostr client. Cannot proceed: {e}");
            // Clean up any resources if needed
            exit(1)
        }
    };

    // Client subscription
    client.subscribe(subscription).await?;

    // Publish NIP-01 kind 0 metadata event
    let mostro_settings = Settings::get_mostro();
    let mut has_metadata = false;
    let mut metadata = Metadata::new();

    if let Some(ref name) = mostro_settings.name {
        metadata = metadata.name(name);
        has_metadata = true;
    }
    if let Some(ref about) = mostro_settings.about {
        metadata = metadata.about(about);
        has_metadata = true;
    }
    if let Some(ref picture) = mostro_settings.picture {
        if let Ok(url) = Url::parse(picture) {
            metadata = metadata.picture(url);
            has_metadata = true;
        } else {
            tracing::warn!("Invalid picture URL in settings: {}", picture);
        }
    }
    if let Some(ref website) = mostro_settings.website {
        if let Ok(url) = Url::parse(website) {
            metadata = metadata.website(url);
            has_metadata = true;
        } else {
            tracing::warn!("Invalid website URL in settings: {}", website);
        }
    }

    if has_metadata {
        if let Ok(metadata_ev) = metadata.finalize(mostro_keys) {
            let _ = client.send_event(&metadata_ev).await;
            tracing::info!("Published NIP-01 kind 0 metadata event");
        }
    }

    // Cashu escrow mode (docs/cashu/, CF-5): run the daemon with NO Lightning
    // node. Skip `LndConnector::new()` and the LN status probe entirely,
    // connect the configured mints instead, attach them to the context, and
    // hand off to the Cashu event loop. Makers choose a mint per order, so an
    // unreachable mint is a warning, not a reason to refuse to boot: orders
    // on it fail to lock until it is back (issue #1046). Every trade action is still rejected with
    // `CantDo(InvalidAction)` until the feature tracks land. The default
    // Lightning path below is left byte-for-byte unchanged.
    if Settings::is_cashu_enabled() {
        // The `mint_urls` entries were validated at config load (CF-1); this
        // expect is unreachable for a validated config.
        let mint_urls = Settings::get_cashu()
            .map(|c| c.mint_urls.clone())
            .expect("cashu enabled but [cashu] settings missing after validation");
        if mint_urls.is_empty() {
            tracing::info!(
                "Starting in Cashu escrow mode — any mint accepted (LND not initialised)"
            );
        } else {
            tracing::info!(
                "Starting in Cashu escrow mode — connecting mints {} (LND not initialised)",
                mint_urls.join(", ")
            );
        }
        let cashu_mints = Arc::new(cashu::mints::CashuMints::connect_configured(&mint_urls).await);

        // The admin gRPC server takes a Lightning client that Cashu mode never
        // initialises, so it is not started here. Warn (rather than silently
        // skip) when an operator has RPC enabled, so the missing API is not a
        // surprise. Starting the LN-independent RPC subset in Cashu mode is a
        // follow-up (see PR #828 review).
        if RpcServer::is_enabled() {
            tracing::warn!(
                "[rpc].enabled = true but the admin gRPC server is NOT started in Cashu mode: \
                 it requires a Lightning client Cashu mode does not initialise. Disable [rpc], \
                 or run in Lightning mode, if you need the RPC API."
            );
        }

        // Warm the anti-spam gate exactly as the Lightning path does.
        install_spam_gate().await;

        let settings = Arc::new(
            MOSTRO_CONFIG
                .get()
                .expect("MOSTRO_CONFIG not initialized")
                .clone(),
        );
        let maintenance = MaintenanceState::load(get_db_pool().as_ref()).await?;
        if maintenance.is_enabled() {
            tracing::warn!("Maintenance mode is ON: new orders and takes are rejected");
        }
        let ctx = AppContext::new(
            get_db_pool(),
            client.clone(),
            settings,
            MESSAGE_QUEUES.queue_order_msg.clone(),
            mostro_keys.clone(),
        )
        .with_cashu_mints(cashu_mints)
        .with_maintenance(maintenance);

        start_scheduler(ctx.clone()).await;

        // Run the Mostro Cashu event loop and be happy!!
        return run_cashu(ctx).await;
    }

    let mut ln_client = LndConnector::new().await?;
    let ln_status = ln_client.get_node_info().await?;
    let ln_status = LnStatus::from_get_info_response(ln_status);
    let node_pubkey = ln_status.node_pubkey.clone();
    if LN_STATUS.set(ln_status).is_err() {
        panic!("No connection to LND node - shutting down Mostro!");
    };

    // Node-identity guard: hold invoices live only in the node that issued
    // them, so starting against a different node while escrow is still open
    // on the old one would strand every release/cancel. Refuse loudly here
    // instead of failing one order at a time (spec §3.6).
    // Off mainnet the dev fee job never runs (#1039), so release the claims
    // an interrupted run left behind before the guard below counts them as
    // in-flight dev fees.
    let networks = LN_STATUS
        .get()
        .map(|status| status.networks.clone())
        .unwrap_or_default();
    if !dev_fee_payments_enabled(&networks) {
        match release_all_pending_claims(get_db_pool().as_ref()).await {
            Ok(0) => {}
            Ok(released) => tracing::info!(
                "Released {released} interrupted dev fee claim(s) left by a previous run"
            ),
            Err(e) => tracing::warn!("Failed to release interrupted dev fee claims: {e}"),
        }
    }

    let allow_node_change = Settings::get_ln().allow_node_change;
    match node_identity_guard(get_db_pool().as_ref(), &node_pubkey, allow_node_change).await? {
        NodeIdentityDecision::FirstBoot => {
            tracing::info!("Recorded Lightning node identity {node_pubkey}");
        }
        NodeIdentityDecision::Same => {}
        NodeIdentityDecision::ChangedDrained { previous } => {
            tracing::warn!(
                "Lightning node changed from {previous} to {node_pubkey} with no open escrow; recorded"
            );
        }
        NodeIdentityDecision::ChangedOverridden { previous, counters } => {
            tracing::warn!(
                "Lightning node changed from {previous} to {node_pubkey} with open escrow \
                 ({counters:?}) — [lightning].allow_node_change = true, continuing. Disputed \
                 escrow can still be closed with AdminCancel (the old-node HTLC refunds the \
                 seller at CLTV expiry) and its bonds released; settled escrow, bond slashes \
                 and range maker bond closes bound to the old node CANNOT be executed by the \
                 daemon and stay Locked, see docs/MAINTENANCE_MODE_LN_MIGRATION.md §5.1"
            );
        }
        NodeIdentityDecision::ChangedWithOpenEscrow { previous, counters } => {
            tracing::error!(
                "REFUSING TO START: Lightning node changed from {previous} to {node_pubkey} but \
                 escrow is still bound to the old node: {counters:?}. Reconnect the old node and \
                 drain it (SetMaintenanceMode + GetMaintenanceStatus until drained == true), or \
                 if it is gone for good follow docs/MAINTENANCE_MODE_LN_MIGRATION.md §5.1 and set \
                 [lightning].allow_node_change = true."
            );
            std::process::exit(1);
        }
    }

    // A failure here means no in-flight hold invoice is resubscribed for the
    // whole run, which is indistinguishable from "there were none" unless it is
    // said out loud — the same silent-failure shape this path already had.
    match find_held_invoices(get_db_pool().as_ref()).await {
        Err(e) => tracing::error!(
            "Could not load held invoices to resubscribe; in-flight trades will \
             not be observed until the next restart: {e}"
        ),
        Ok(held_invoices) => {
            for invoice in held_invoices.iter() {
                if let Some(hash) = &invoice.hash {
                    // `orders.hash` is hex text; LND's SubscribeSingleInvoiceRequest
                    // wants the 32 raw bytes. Passing the 64 ASCII characters makes
                    // every resubscribe fail, which is silent here because the error
                    // is only logged — and a missed subscription looks exactly like
                    // a seller who never paid.
                    let r_hash = match crate::lightning::decode_hash32("order hash", hash) {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            tracing::error!("Order {} has an undecodable hash: {e}", invoice.id);
                            continue;
                        }
                    };
                    tracing::info!("Resubscribing order id - {}", invoice.id);
                    if let Err(e) = invoice_subscribe(r_hash, None).await {
                        tracing::error!("Ln node error {e}")
                    }
                }
            }
        }
    }

    // Resubscribe to any in-flight anti-abuse bond hold invoices so a
    // restart doesn't strand a taker who paid the bond just before the
    // daemon went down. Inert when the feature is disabled.
    let bond_pool = get_db_pool();
    if let Err(e) = app::bond::resubscribe_active_bonds(&bond_pool).await {
        tracing::warn!("Failed to resubscribe active bonds: {e}");
    }

    // Maintenance (drain) flag: one instance shared by the admin RPC (which
    // flips it) and the event loop's AppContext (which gates on it).
    let maintenance = MaintenanceState::load(get_db_pool().as_ref()).await?;
    if maintenance.is_enabled() {
        tracing::warn!("Maintenance mode is ON: new orders and takes are rejected");
    }

    // Start RPC server if enabled
    if RpcServer::is_enabled() {
        let rpc_server = RpcServer::new();
        let rpc_keys = mostro_keys.clone();
        let rpc_pool = get_db_pool();
        let rpc_ln_client = Arc::new(tokio::sync::Mutex::new(ln_client.clone()));
        let rpc_maintenance = maintenance.clone();

        tokio::spawn(async move {
            match rpc_server
                .start(rpc_keys, rpc_pool, rpc_ln_client, rpc_maintenance)
                .await
            {
                Ok(_) => tracing::info!("RPC server started successfully"),
                Err(e) => tracing::error!("RPC server failed to start: {}", e),
            }
        });
    }

    // Install the protocol-v2 anti-spam gate and warm its active-trade-pubkey
    // cache before the event loop starts (mode-agnostic — both `run` and
    // `run_cashu` consult it for kind-14 events).
    install_spam_gate().await;

    // Build AppContext explicitly with all dependencies
    let settings = Arc::new(
        MOSTRO_CONFIG
            .get()
            .expect("MOSTRO_CONFIG not initialized")
            .clone(),
    );
    let ctx = AppContext::new(
        get_db_pool(),
        client.clone(),
        settings,
        MESSAGE_QUEUES.queue_order_msg.clone(),
        mostro_keys.clone(),
    )
    .with_maintenance(maintenance);

    // Start scheduler for tasks
    start_scheduler(ctx.clone()).await;

    // Run the Mostro and be happy!!
    run(ctx, &mut ln_client).await
}

/// Install the protocol-v2 anti-spam gate and warm its active-trade-pubkey
/// cache before the event loop starts, so the very first kind-14 events are
/// already pre-filtered against known keys (spec §6 Phase 2). The cache is
/// kept fresh afterwards by `job_refresh_active_pubkeys`.
///
/// Shared by both boot paths (Lightning `run` and Cashu `run_cashu`, CF-5) so
/// the warm-up logic exists in exactly one place.
async fn install_spam_gate() {
    use crate::spam_gate::{SpamGate, REPLAY_WINDOW_SECS};
    let gate = SpamGate::new(REPLAY_WINDOW_SECS);
    match db::find_active_trade_pubkeys(get_db_pool().as_ref()).await {
        Ok(keys) => {
            tracing::info!(
                "SpamGate: warming active-trade-pubkey cache ({} keys)",
                keys.len()
            );
            gate.set_known(keys);
        }
        Err(e) => tracing::warn!("SpamGate: initial cache warm failed: {e}"),
    }
    if gate.install_global().is_err() {
        tracing::warn!("SpamGate already installed");
    }
}

/// Build the multi-source [`crate::price::PriceManager`] from settings and
/// install it as the process-wide global. When `[price]` is absent in the
/// settings file we synthesise it from the legacy `[mostro]` keys
/// (`bitcoin_price_api_url`, `exchange_rates_update_interval_seconds`,
/// `publish_exchange_rates_to_nostr`) so existing `settings.toml` files keep
/// working byte-for-byte (spec §10.1).
fn install_price_manager() -> std::result::Result<(), Box<dyn std::error::Error>> {
    use crate::price::{synthesise_legacy_price_settings, PriceManager};

    let mostro_settings = Settings::get_mostro();
    let price_settings = match Settings::get_price() {
        Some(p) => {
            // Multi-source mode: the `[price.providers.*]` tables drive
            // aggregation, so the legacy `[mostro].bitcoin_price_api_url` is
            // not consulted here. Surface that explicitly so an operator who
            // still has the legacy key set isn't misled into thinking it
            // takes effect — name the providers actually in play instead.
            let mut enabled: Vec<&str> = p
                .providers
                .iter()
                .filter(|(_, cfg)| cfg.enabled)
                .map(|(id, _)| id.as_str())
                .collect();
            enabled.sort_unstable();
            let enabled = if enabled.is_empty() {
                "<none>".to_string()
            } else {
                enabled.join(", ")
            };
            tracing::warn!(
                "price: legacy `bitcoin_price_api_url` = \"{}\" is ignored for price \
                 aggregation because `[price]` is configured; using enabled providers: {}",
                mostro_settings.bitcoin_price_api_url,
                enabled,
            );
            p.clone()
        }
        None => synthesise_legacy_price_settings(
            &mostro_settings.bitcoin_price_api_url,
            mostro_settings.exchange_rates_update_interval_seconds,
            mostro_settings.publish_exchange_rates_to_nostr,
        ),
    };

    let manager = PriceManager::from_settings(price_settings)
        .map_err(|e| -> Box<dyn std::error::Error> { format!("price: {e}").into() })?;
    manager
        .install_global()
        .map_err(|e| -> Box<dyn std::error::Error> { format!("price: {e}").into() })?;
    tracing::info!("PriceManager installed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use mostro_core::message::Message;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_message_deserialize_serialize() {
        let sample_message = r#"{"order":{"version":1,"request_id":1,"trade_index":null,"id":"7dd204d2-d06c-4406-a3d9-4415f4a8b9c9","action":"fiat-sent","payload":null}}"#;
        let message = Message::from_json(sample_message).unwrap();
        assert!(message.verify());
        let json_message = message.as_json().unwrap();
        assert_eq!(sample_message, json_message);
    }

    #[test]
    fn test_wrong_message_should_fail() {
        let sample_message = r#"{"order":{"version":1,"request_id":1,"action":"take-sell","payload":{"order":{"kind":"sell","status":"pending","amount":100,"fiat_code":"XXX","fiat_amount":10,"payment_method":"SEPA","premium":1,"buyer_invoice":null,"created_at":1640839235}}}}"#;
        let message = Message::from_json(sample_message).unwrap();
        assert!(!message.verify());
    }

    #[test]
    fn test_fee_rounding() {
        let fee = 0.003 / 2.0;

        let mut amt = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_micros() as i64;

        // Test 1000 "random" amounts
        for _i in 1..=1000 {
            let fee_calculated = fee * amt as f64;
            let rounded_fee = fee_calculated.round();
            // Seller side
            let seller_total_amt = rounded_fee as i64 + amt;
            assert_eq!(amt, seller_total_amt - rounded_fee as i64);
            // Buyer side

            let buyer_total_amt = amt - rounded_fee as i64;
            assert_eq!(amt, buyer_total_amt + rounded_fee as i64);

            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_millis() as i64;

            amt %= 100_000_i64;
            amt *= (rounded_fee as i64) % 100_i64;
            amt += nonce;
        }
    }

    #[test]
    fn test_debug_log_level_setting() {
        // Test the logical flow of log level setting
        // We can't test the actual environment variable setting since main() has already run

        let debug_log_setting = if cfg!(debug_assertions) {
            "error,mostro=info"
        } else {
            "none,mostro=info"
        };

        // Verify the log settings are correctly defined
        assert!(!debug_log_setting.is_empty());
        assert!(debug_log_setting.contains("mostro=info"));

        if cfg!(debug_assertions) {
            assert!(debug_log_setting.contains("error"));
        } else {
            assert!(debug_log_setting.contains("none"));
        }
    }
}
