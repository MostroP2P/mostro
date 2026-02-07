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

**Directory setup**:
- Creates `~/.mostro/` directory if not exists
- Checks for existing `~/.mostro/settings.toml`
- If missing: copies `settings.tpl.toml` to `~/.mostro/settings.toml`
- On first run after creating the file, the process exits so the user can edit `settings.toml`, then restart Mostro.
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
4) NIP-01 Kind 0 Metadata: If any metadata fields (`name`, `about`, `picture`, `website`) are configured, publishes a kind 0 metadata event so clients can display the Mostro instance's profile.
5) LND: `LndConnector::new()` + `get_node_info()` → `config::LN_STATUS`.
6) Held invoices: `db::find_held_invoices()` → resubscribe via `util::invoice_subscribe`.
7) RPC: start if `rpc.enabled`.
8) Scheduler: `scheduler::start_scheduler()`.
9) Scheduler jobs: Payment retry job configured with `payment_retries_interval`.
10) Event loop: `app::run(keys, client, ln_client)`.

## Settings Structure

**File**: `src/config/settings.rs`, types in `src/config/types.rs`.

Configuration is loaded from `~/.mostro/settings.toml` (template: `settings.tpl.toml`). Values shown below come from the template and indicate the required keys that must exist in `settings.toml`. Some fields have Rust Default implementations; however, the daemon still expects these keys to be present in `settings.toml`. If a key relies on a Rust Default and is present but empty or omitted by tooling, the daemon falls back to the Rust Default value.

### Configuration Sections:

**Database** (`src/config/types.rs:21-26`):
- `url` (String): Database connection URL
  - Example: `"sqlite://mostro.db"` or `"postgres://user:pass@localhost/dbname"`
  - Default: `"sqlite://mostro.db"`

**Nostr** (`src/config/types.rs:47-54`):
- `nsec_privkey` (String): Mostro's Nostr private key in nsec format
- `relays` (Vec<String>): List of Nostr relay URLs for event broadcasting
  - Default: `['ws://localhost:7000']`
  - Note: At least one relay required

**Lightning** (`src/config/types.rs:27-46`):
- `lnd_cert_file` (String): Path to LND TLS certificate
- `lnd_macaroon_file` (String): Path to LND macaroon auth file
- `lnd_grpc_host` (String): LND gRPC endpoint URL
- `invoice_expiration_window` (u32): Required invoice validity window in seconds (default: 3600)
- `hold_invoice_cltv_delta` (u32): Hold invoice CLTV delta in blocks (default: 144)
- `hold_invoice_expiration_window` (u32): Hold invoice expiration in seconds (default: 300)
- `payment_attempts` (u32): Max payment retry attempts (default: 3)
- `payment_retries_interval` (u32): Retry interval in seconds (default: 60)

**Mostro** (`src/config/types.rs:76-108`):

*Fee Configuration:*
- `fee` (f64): Mostro trading fee percentage (default: 0)
- `max_routing_fee` (f64): Maximum routing fee percentage; 0.001 = 0.1% (default: 0.001)

*Order Limits:*
- `max_order_amount` (u32): Maximum order amount in satoshis (default: 1000000)
- `min_payment_amount` (u32): Minimum payment amount in satoshis (default: 100)
- `max_orders_per_response` (u8): Maximum orders returned in single response (default: 10)

*Expiration Settings:*
- `expiration_hours` (u32): Order expiration in hours (default: 24)
- `max_expiration_days` (u32): Maximum allowed expiration in days (default: 15)
- `expiration_seconds` (u32): Pending order expiration in seconds (default: 900)

*Publishing Intervals:*
- `publish_relays_interval` (u32): Relay list event interval in seconds (default: 60)
- `user_rates_sent_interval_seconds` (u32): User rate events interval in seconds (default: 3600)
- `publish_mostro_info_interval` (u32): Mostro info publish interval in seconds (default: 300)

*Network/API:*
- `pow` (u8): Proof-of-work difficulty requirement (default: 0)
- `bitcoin_price_api_url` (String): Bitcoin price API base URL (default: [`https://api.yadio.io`](https://api.yadio.io))

*Market Support:*
- `fiat_currencies_accepted` (Vec<String>): Accepted fiat currencies; empty list accepts all (default: ['USD', 'EUR', 'ARS', 'CUP'])

*NIP-01 Kind 0 Metadata (optional):*
- `name` (Option\<String\>): Human-readable name for this Mostro instance (default: None)
- `about` (Option\<String\>): Short description of this Mostro instance (default: None)
- `picture` (Option\<String\>): URL to avatar image, recommended square max 128x128px (default: None)
- `website` (Option\<String\>): Operator website URL (default: None)

**RPC** (`src/config/types.rs:55-74`):
- `enabled` (bool): Enable RPC server (Rust Default: false)
- `listen_address` (String): Bind address (Rust Default: "127.0.0.1")
- `port` (u16): Listen port (Rust Default: 50051)
- Note: These fields have a Rust Default implementation, but `settings.toml` must still include these keys. If a key is present but empty or omitted by tooling, the daemon falls back to the Rust Default value.

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
