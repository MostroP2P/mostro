# Startup and Configuration

This guide explains Mostro’s boot sequence and configuration surfaces.

## Overview
- Entry: `src/main.rs:1`
- Initializes logging (RUST_LOG), settings, DB, Nostr client, LND connector, RPC (optional), scheduler.
- Resubscribes held invoices, then calls `app::run`.

## Pre-Boot Initialization

Before settings initialization, the daemon performs (see `src/main.rs`):

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
- Creates `~/.mostro/` directory if not exists, owner-only (mode `0700` on Unix)
- Checks for existing `~/.mostro/settings.toml`
- If missing: copies `settings.tpl.toml` to `~/.mostro/settings.toml`, owner-only
  (mode `0600` on Unix), since the file receives `nsec_privkey` once edited
- On first run after creating the file, the process exits so the user can edit `settings.toml`, then restart Mostro.
- Overrides database URL: `~/.mostro/mostro.db`
**Settings loading**:
- Parses TOML into Settings struct
- Stores in global `config::MOSTRO_CONFIG` via `init_mostro_settings()`
- Accessible via `Settings::get_*()` methods throughout application

### Database Connection (db::connect)

**Source**: `src/db.rs`, function `connect()`.

**Initialization**:

1. **New database creation**:
   - Detects if database file exists
   - If new: runs all migrations from `migrations/` directory
   - Creates tables, indexes, and schema

2. **Legacy migrations**:
   - Performs column migrations for backwards compatibility
   - Example: disputes table structure updates

3. **Connection pooling**:
   - Creates `SqlitePool` with configured connection limits
   - Stores in global `config::DB_POOL`

**Note:** Database encryption has been removed; no password is used for the database.

**Error handling**: Database connection errors halt startup

### Additional Boot Steps

1) Settings init: `cli::settings_init()` loads `settings.toml` (template: `settings.tpl.toml`).
2) DB connect: `db::connect()` sets `config::DB_POOL`.
3) Nostr: `util::connect_nostr()` sets `config::NOSTR_CLIENT`.
4) NIP-01 Kind 0 Metadata: If any metadata fields (`name`, `about`, `picture`, `website`) are configured, publishes a kind 0 metadata event so clients can display the Mostro instance's profile.
5) LND: `LndConnector::new()` + `get_node_info()` → `config::LN_STATUS`.
6) Held invoices: `db::find_held_invoices()` → resubscribe via `util::invoice_subscribe`.
7) RPC: start if `rpc.enabled`.
8) AppContext: Build `AppContext` with pool, client, settings, message queue, and keys.
9) Scheduler: `scheduler::start_scheduler(ctx)` — receives `AppContext` for dependency injection.
10) Event loop: `app::run(ctx, ln_client)` — receives `AppContext` instead of individual dependencies.

## Settings Structure

**File**: `src/config/settings.rs`, types in `src/config/types.rs`.

Configuration is loaded from `~/.mostro/settings.toml` (template: `settings.tpl.toml`). Values shown below come from the template and indicate the required keys that must exist in `settings.toml`. Some fields have Rust Default implementations; however, the daemon still expects these keys to be present in `settings.toml`. If a key relies on a Rust Default and is present but empty or omitted by tooling, the daemon falls back to the Rust Default value.

### Configuration Sections:

**Database** (`src/config/types.rs:21-26`):
- `url` (String): Database connection URL (Mostro uses SQLite)
  - Example (relative to the process working directory): `"sqlite://mostro.db"`
  - Example (absolute path; use a real path — **do not** use `~`; SQLx does not expand tilde): `"sqlite:///home/youruser/.mostro/mostro.db"`
  - Default: `"sqlite://mostro.db"`

**Nostr** (`src/config/types.rs`):
- `nsec_privkey` (String): Mostro's Nostr private key in nsec format.
  - Can be overridden by the `MOSTRO_NSEC_PRIVKEY` environment variable
    (env var takes precedence; whitespace-only values are ignored).
  - Mostro also auto-loads `<settings_dir>/.env` at startup (e.g.
    `~/.mostro/.env`) so the variable can live in a separate file with
    restricted permissions.
  - Precedence: real env var > `<settings_dir>/.env` > `settings.toml`.
- `relays` (Vec<String>): List of Nostr relay URLs for event broadcasting
  - Default: `['ws://localhost:7000']`
  - Note: At least one relay required

**Lightning** (`src/config/types.rs:27-46`):
- `lnd_cert_file` (String): Path to LND TLS certificate
- `lnd_macaroon_file` (String): Path to LND macaroon auth file
  - The admin macaroon is spend-capable: any account that can read it controls
    the node, including the funds escrowed in Mostro's hold invoices. Keep it
    readable only by the user running `mostrod` (`chmod 600`).
  - At startup (Lightning mode only, right before the LND connection is opened)
    `src/config/permissions.rs`, `fn warn_if_other_accessible` logs a warning when the
    file's `other` permission bits are set, and suggests `chmod o=`. The check
    is advisory — the daemon still starts — and tolerates `0640`, the mode LND
    itself writes the macaroon with, so reading it through the node's group
    stays supported.
  - The same check runs over `settings.toml`, `<settings_dir>/.env` and
    `<settings_dir>/mostro.db` right after the settings load, in both Lightning and
    Cashu mode: the first two carry `nsec_privkey`, which is the instance's identity
    regardless of which escrow it uses, and the database holds the trade history, the
    disputes and the hold-invoice preimages. SQLite creates the database under the
    umask, so a settings directory that predates the `0700` default is where a `0644`
    database comes from. A missing file is silent: `.env` is optional, and the database
    does not exist yet on a first boot.
  - Only the mode bits are inspected: a POSIX ACL can grant a named user access
    without setting them, so a quiet startup is not proof that no other account
    can read the file.
- `lnd_grpc_host` (String): LND gRPC endpoint URL
- `invoice_expiration_window` (u32): Required invoice validity window in seconds (default: 3600)
- `hold_invoice_cltv_delta` (u32): Hold invoice CLTV delta in blocks (default: 144)
- `hold_invoice_expiration_window` (u32): Hold invoice expiration in seconds (default: 300)
- `payment_attempts` (u32): Max payment retry attempts (default: 3)
- `payment_retries_interval` (u32): Retry interval in seconds (default: 60)
- `max_final_cltv_expiry_delta` (u32): Upper bound, in blocks, on the `min_final_cltv_expiry_delta` of a user-supplied payout invoice (buyer payout, bond payout, dev fee). It bounds how long that payee can hold the outgoing HTLC without settling — the node cannot cancel a locked-in HTLC. 144 (~1 day) is the top of the range real wallets ask for; raise it only if a legitimate wallet is rejected, and never set it to 0 (the check is `delta > bound`, and BOLT11 substitutes 18 when the field is absent, so 0 rejects every invoice) (default: 144)
- `allow_node_change` (bool): Disaster-recovery override for the boot node-identity guard. By default mostrod refuses to start when the connected LND's identity pubkey differs from the one it last ran with and escrow is still open on the old node; `true` downgrades that to a warning and knowingly leaves those trades unresolved (default: false; see `docs/MAINTENANCE_MODE_LN_MIGRATION.md` §3.6/§5.1)
- `escrow_deadline_margin_blocks` (u32): Safety margin in blocks before the hold invoice's CLTV horizon; at `hold_invoice_cltv_delta - escrow_deadline_margin_blocks` blocks after the escrow was paid, mostrod cancels the trade (Active) or opens a dispute (FiatSent) before LND auto-refunds the escrow. Must exceed the LND node's `invoices.holdexpirydelta` (LND default: 12) (default: 24)
- `max_inflight_payouts` (u32): Ceiling on how many payments LND may have in flight before a new payout waits its turn instead of adding another unresolved HTLC. A payee that never settles pins the sats, the routing liquidity behind them and an HTLC slot until the CLTV expires, and the sender cannot cancel a locked-in HTLC. A gated payout is delayed, never dropped: its claim is heartbeated while it waits and it dispatches once the node has room. The count comes from LND plus the slots dispatchers in this process have already booked, never from the payout rows, so payouts still queued behind the gate do not inflate it and concurrent dispatchers cannot slip past it together. The gate is checked before a dispatch permit is taken, so a held payout never blocks payouts to other destinations, and it applies to bond payouts as well (a held bond payout is retried on the next scheduler tick). A node that does not answer the count within 10 s is treated like an unreachable one: the gate fails open. `0` disables the gate (default: 100)
- `max_inflight_payouts_per_destination` (u32): The same ceiling applied per payout destination, and the sharper of the two. Holding an HTLC requires controlling the node that receives it, so the HTLCs an abusive payee refuses to settle all share a destination pubkey; capping there catches it long before the node-wide total would move, and prices the attack in funded channels (one more destination node per this many held HTLCs). Ten simultaneously unresolved payments to one pubkey already means that destination is not settling — the benign case the default leaves room for is several buyers cashing out at once to the same custodial wallet. `0` disables this arm of the gate (default: 10)
- `payment_cltv_limit` (u32): Upper bound, in blocks, on the *total* timelock of a payout route, handed to LND as `SendPaymentRequest.cltv_limit`. `max_final_cltv_expiry_delta` bounds only the payee's own hop; this bounds the whole path, which is what the node's channel is locked for when a hop force-closes and the HTLC has to resolve on-chain. Omitted from `settings.toml` it takes the default; set to `0` to send no ceiling and let the node apply its own `--max-cltv-expiry` (2016 by default). It must not exceed that node setting — LND refuses a larger `cltv_limit` outright rather than clamping it, and every payout then retries without the ceiling and logs an error. It must also stay at least `MIN_ROUTE_CLTV_HEADROOM` (576) blocks above `max_final_cltv_expiry_delta`, or pathfinding fails honest payouts with "no route"; a smaller non-zero value is refused and deferred to the node with an error logged, deliberately not raised to that floor, since inventing a ceiling could push it past the node's maximum (default: 1008)

**Mostro** (`src/config/types.rs:76-108`):

*Fee Configuration:*
- `fee` (f64): Mostro trading fee percentage (default: 0)
- `max_routing_fee` (f64): Maximum routing fee percentage; 0.002 = 0.2% (default: 0.002)

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
- `pow` (u8): Proof-of-work difficulty (leading-zero bits, NIP-13) required of every incoming event, checked on the outer event before anything else (default: 0, i.e. no requirement)
- `pow_first_contact` (Option\<u8\>): Stiffer PoW demanded of a *first-contact* event — one whose visible sender is not in the active-trade cache — checked before the NIP-44 decrypt. Only enforced on the `nip44` transport; `None` falls back to `pow` (default: None). Setting it *below* `pow` has no effect, since the base check runs first. See [TRANSPORT_V2_SPEC.md](TRANSPORT_V2_SPEC.md) §6 Phase 2
- `active_pubkeys_refresh_interval` (u64): How often, in seconds, to rebuild the active-trade-pubkey cache that the first-contact gate consults (default: 60)
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
- `auth_token` (String, optional): Shared secret required as `authorization: Bearer <token>` on the mutating admin RPCs; unset means no application-layer auth (see `docs/RPC.md`)
- Note: These fields have a Rust Default implementation, but `settings.toml` must still include these keys. If a key is present but empty or omitted by tooling, the daemon falls back to the Rust Default value.

## Global Variables

**Source**: `src/config/mod.rs`

```rust
pub static MOSTRO_CONFIG: OnceLock<Settings> = OnceLock::new();
pub static NOSTR_CLIENT: OnceLock<Client> = OnceLock::new();
pub static LN_STATUS: OnceLock<LnStatus> = OnceLock::new();
pub static DB_POOL: OnceLock<Arc<sqlx::SqlitePool>> = OnceLock::new();

pub static MESSAGE_QUEUES: LazyLock<MessageQueues> =
    LazyLock::new(MessageQueues::default);
```

(`MessageQueues` holds `Arc<RwLock<…>>` queues for order messages, cant-do messages, rating events, and restore-session messages.)

There is **no** database password or separate global for SQLite; the daemon opens the file URL from `[database]` in `settings.toml` only.

**Access patterns**:
- `Settings::get_mostro()` → Mostro settings
- `Settings::get_ln()` → Lightning settings
- `Settings::get_rpc()` → RPC settings
- Database: `config::DB_POOL.get().unwrap()`
- Nostr: `config::NOSTR_CLIENT.get().unwrap()`

## Commands
- Build: `cargo build`
- Run: `cargo run`
- Migrations: applied automatically on connect; manual `sqlx migrate run` optional when using `sqlx-cli`.

## Security
- Do not commit populated `settings.toml`.
- Keep templates in `settings.tpl.toml`; place runtime config at `~/.mostro/settings.toml`.
- `settings.toml` carries `nsec_privkey` in plaintext unless it is supplied through
  `MOSTRO_NSEC_PRIVKEY`, and the settings directory also holds `mostro.db` and, in the
  Docker flows, the LND credentials under `lnd/`. Keep the directory at mode `0700` and
  the file at `0600`.
- Every path that creates them does so already, through one of three primitives in
  `src/config/permissions.rs`: `fn create_settings_dir` for the directory,
  `fn create_owner_only` for a file that must not already exist, and
  `fn write_owner_only_atomic` for one that is rewritten. A directory or file created
  outside the daemon — `mkdir`, `cp`, `curl` — inherits the umask instead, so the
  deployment guides use `install -d -m 700` and `install -m 600`.
- `create_owner_only`'s three callers are the non-interactive template copy
  (`src/config/util.rs`, `fn init_configuration_file`), the manual template copy
  (`src/config/wizard.rs`, `fn run_setup_menu`) and the guided wizard save
  (`src/config/wizard.rs`, `fn save_settings`). `write_owner_only_atomic` has one:
  the wizard's `.env` write (`src/config/wizard.rs`, `fn write_env_file`).
- An existing settings directory is left as it is, so a deliberately group-readable
  deployment keeps working. `0700` also applies to the settings directory alone — any
  missing parents are created under the umask, the way `mkdir -p` would, so
  `mostrod -d /srv/apps/mostro/conf` on a fresh tree closes off `conf` without closing
  off `/srv/apps` for anything else that lives there.
- `create_owner_only` uses `O_CREAT | O_EXCL`, so it fails rather than write through
  whatever already occupies the path. This covers initial creation only: on a settings
  directory another local account can write to, a symlink planted between the caller's
  existence check and the create would otherwise have its target truncated and its mode
  reset to `0600`. A write that fails partway removes the file it created, so a full
  disk cannot leave a truncated `settings.toml` behind for the next boot to reject as
  malformed TOML.
- `write_env_file` cannot refuse an existing path the way `create_owner_only` does —
  rewriting an existing `.env` is a supported thing to do — so
  `write_owner_only_atomic` stages the line in a fresh `O_EXCL` temporary beside the
  target and `rename`s it into place. `rename` never opens the destination, so a
  planted symlink is replaced rather than followed and its target keeps both its
  contents and its mode. The temporary is `fsync`ed before the rename and the directory
  after it (`src/config/permissions.rs`, `fn sync_dir`), so the replacement is durable
  and not merely atomic — a power loss right after an unflushed rename can leave the
  directory entry pointing at the old file, which for `.env` means losing an
  `nsec_privkey` the wizard reported as saved. A failed flush is logged and not
  propagated: the contents are already in place. `.env` matters here as much as
  `settings.toml`: it carries the same `nsec_privkey`, and in the wizard flow it is
  written first.
- It is not a check on startup as a whole. A `settings.toml` that already exists is read
  and loaded normally, symlink or not — `fn init_configuration_file` only reaches the
  creation path when it finds no settings file at all.
- The guarantee covers the final entry, not the directory path leading to it. The
  parents `fn create_settings_dir` has to invent are made with `fs::create_dir_all`,
  which resolves symlinked components on the way, so a settings directory
  reached through a symlinked parent is created at whatever that link points to. This is
  deliberate: operators do symlink a config directory onto another volume, and planting
  such a link on the default `~/.mostro` path means write access to `$HOME`, which is a
  compromise of the account already. A settings directory that is itself a symlink to an
  existing directory is used as-is, and a dangling one is refused.
