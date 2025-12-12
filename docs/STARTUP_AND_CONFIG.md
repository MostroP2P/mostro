# Startup and Configuration

This guide explains Mostro’s boot sequence and configuration surfaces.

## Overview
- Entry: `src/main.rs:1`
- Initializes logging (RUST_LOG), settings, DB, Nostr client, LND connector, RPC (optional), scheduler.
- Resubscribes held invoices, then calls `app::run`.

## Pre-Boot Initialization

**Lines 33-48 in src/main.rs**:

Before settings initialization, the daemon performs:

1. **Screen clearing**: Clears terminal for clean output
2. **Logging setup**:
   ```rust
   let rust_log = if cfg!(debug_assertions) {
       "debug"
   } else {
       env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string())
   };
   env::set_var("RUST_LOG", rust_log);
   pretty_env_logger::init();
   ```
3. **Debug/Release mode detection**: Sets appropriate log level

## Boot Steps

### Settings Initialization Details

**Function flow**:
1. `cli::settings_init()` (cli.rs:47)
2. Calls `config::util::init_configuration_file()` (config/util.rs:16)

**Directory setup**:
- Creates `~/.mostro/` directory if not exists
- Checks for existing `~/.mostro/settings.toml`
- If missing: copies `settings.tpl.toml` to `~/.mostro/settings.toml`
- Overrides database URL: `~/.mostro/mostro.db`

**Settings loading**:
- Parses TOML into Settings struct
- Stores in global `config::MOSTRO_CONFIG` via `init_mostro_settings()`
- Accessible via `Settings::get_*()` methods throughout application

### Database Connection (db::connect)

**Source**: `src/db.rs:480`

**Complex initialization process**:

1. **New database creation**:
   - Detects if database file exists
   - If new: runs all migrations from `migrations/` directory
   - Creates tables, indexes, and schema

2. **Password encryption handling**:
   - Checks if database is encrypted
   - If encrypted: prompts for password interactively
   - Validates password against stored hash
   - Stores decrypted password in `config::MOSTRO_DB_PASSWORD`

3. **Legacy migrations**:
   - Performs column migrations for backwards compatibility
   - Example: disputes table structure updates

4. **Connection pooling**:
   - Creates `SqlitePool` with configured connection limits
   - Stores in global `config::DB_POOL`

**Error handling**: Database connection errors halt startup

### Additional Boot Steps

1) Settings init: `cli::settings_init()` loads `settings.toml` (template: `settings.tpl.toml`).
2) DB connect: `db::connect()` sets `config::DB_POOL`.
3) Nostr: `util::connect_nostr()` sets `config::NOSTR_CLIENT`.
4) LND: `LndConnector::new()` + `get_node_info()` → `config::LN_STATUS`.
5) Held invoices: `db::find_held_invoices()` → resubscribe via `util::invoice_subscribe`.
6) RPC: start if `rpc.enabled`.
7) Scheduler: `scheduler::start_scheduler()`.
8) Scheduler jobs: Payment retry job configured with `payment_retries_interval`
9) Event loop: `app::run(keys, client, ln_client)`.

## Settings Structure

**File**: `src/config/settings.rs`, types in `src/config/types.rs`, constants in `src/config/constants.rs`.

### Key Settings Fields:

**Mostro** (`src/config/types.rs`):
- `pow`: Proof-of-work difficulty threshold
- `max_routing_fee`: Maximum routing fee percentage for payments >1000 sats
- `min_payment_amount`: Minimum payment amount in sats
- `invoice_expiration_window`: Required invoice validity window (seconds)

**LN** (`src/config/types.rs:28-46`):
- `lnd_grpc_host`: LND gRPC endpoint
- `lnd_cert_file`: Path to TLS certificate
- `lnd_macaroon_file`: Path to macaroon auth file
- `hold_invoice_cltv_delta`: CLTV delta for hold invoices
- `hold_invoice_expiration_window`: Hold invoice expiration (seconds)
- `payment_attempts`: Max payment retry attempts (default: 3)
- `payment_retries_interval`: Retry interval in seconds (default: 60)

**RPC** (`src/config/types.rs:55-74`):
- `enabled`: Enable RPC server (default: false)
- `listen_address`: Bind address (default: "127.0.0.1")
- `port`: Listen port (default: 50051)

**Constants** (`src/config/constants.rs`):
- `MIN_DEV_FEE_PERCENTAGE`: Minimum development fee
- `MAX_DEV_FEE_PERCENTAGE`: Maximum development fee
- `DEV_FEE_LIGHTNING_ADDRESS`: Lightning address for dev fees

## Global Variables

**Source**: `src/config/mod.rs:26-48`

```rust
// Settings and configuration
pub static MOSTRO_CONFIG: OnceLock<Settings> = OnceLock::new();

// Infrastructure connections
pub static NOSTR_CLIENT: OnceLock<Client> = OnceLock::new();
pub static LN_STATUS: OnceLock<LnStatus> = OnceLock::new();
pub static DB_POOL: OnceLock<Arc<sqlx::SqlitePool>> = OnceLock::new();

// Security
pub static MOSTRO_DB_PASSWORD: OnceLock<String> = OnceLock::new();

// Message routing
pub static MESSAGE_QUEUES: LazyLock<Arc<Mutex<HashMap<String, VecDeque<String>>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));
```

**Access patterns**:
- `Settings::get_mostro()` → Mostro settings
- `Settings::get_ln()` → Lightning settings
- `Settings::get_rpc()` → RPC settings
- Database: `config::DB_POOL.get().unwrap()`
- Nostr: `config::NOSTR_CLIENT.get().unwrap()`

## Commands
- Build: `cargo build`
- Run: `cargo run`
- SQLx offline data: `cargo sqlx prepare -- --bin mostrod`

## Security
- Do not commit populated `settings.toml`.
- Keep templates in `settings.tpl.toml`; place runtime config at `~/.mostro/settings.toml`.
