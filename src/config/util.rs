/// Utility functions for the config module
/// This module provides utility functions for the config module.
/// It includes functions to initialize the default settings directory and create a settings file from the template if it doesn't exist.
/// It also includes functions to add a trailing slash to a path if it doesn't already have one.
use crate::config::constants::{
    ENV_FILENAME, MAX_DEV_FEE_PERCENTAGE, MIN_DEV_FEE_PERCENTAGE, MIN_RPC_TOKEN_DISTINCT_CHARS,
    MIN_RPC_TOKEN_LEN, RPC_TOKEN_ENV_VAR,
};
use crate::config::secret::{read_nsec_env_var, read_rpc_token_env_var};
use crate::config::types::RpcSettings;
use crate::config::wizard;
use crate::config::{init_mostro_settings, Settings};
use mostro_core::error::MostroError::{self, *};
use mostro_core::error::ServiceError;
use secrecy::{ExposeSecret, SecretString};
use std::fs;
use std::io::IsTerminal;
use std::path::PathBuf;
use zeroize::Zeroizing;

const DB_FILENAME: &str = "mostro.db";

/// Serializes every test that mutates or reads the process environment. One
/// lock for the whole environment rather than one per variable: glibc's
/// `setenv` can reallocate `environ` with no synchronization against a
/// concurrent `getenv`, so two tests holding two *different* locks still race.
/// Async-aware because `rpc::server::tests` holds it across `.await`; sync
/// tests take it through `blocking_lock`.
#[cfg(test)]
pub(crate) static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Resolve `[rpc].listen_address` and `[rpc].port` into the address the server
/// binds.
///
/// `SocketAddr` only parses IP literals, with IPv6 bracketed. Hostnames such as
/// `localhost` and bare `::1` are therefore not bindable addresses, however
/// natural they look in a config file.
///
/// `RpcServer::bind` resolves its address through this function too, so a
/// `listen_address` that passes validation is guaranteed to be one `bind` can
/// use: the two must never disagree, or the daemon accepts a config at startup
/// and then dies on it. It lives here rather than in `rpc::server` so the
/// dependency keeps pointing the usual way, `rpc` → `config`.
pub(crate) fn listen_socket_addr(
    listen_address: &str,
    port: u16,
) -> Result<std::net::SocketAddr, String> {
    format!("{listen_address}:{port}")
        .parse::<std::net::SocketAddr>()
        .map_err(|e| {
            format!(
                "Invalid address {listen_address:?}: {e}. Expected an IP literal, with IPv6 \
                 bracketed — for example 127.0.0.1, [::1] or 0.0.0.0. Hostnames such as \
                 \"localhost\" are not resolved."
            )
        })
}

/// Loads the optional `<settings_dir>/.env` file so that values placed there
/// become available through `std::env::var`. Variables already set in the
/// process environment take precedence and are never overwritten.
///
/// Loading errors (malformed file, permission denied, ...) are logged as
/// warnings instead of being silently swallowed, so misconfigured deployments
/// surface the real root cause at startup rather than failing later with an
/// unrelated empty-key error.
fn load_env_file(settings_dir: &std::path::Path) {
    let env_file = settings_dir.join(ENV_FILENAME);
    if !env_file.exists() {
        return;
    }
    if let Err(e) = dotenvy::from_path(&env_file) {
        tracing::warn!(
            "Failed to load environment file {}: {}. Falling back to settings.toml.",
            env_file.display(),
            e
        );
    }
}

/// If the `MOSTRO_NSEC_PRIVKEY` environment variable is set to a non-empty
/// value, override the nsec loaded from `settings.toml`. Whitespace is
/// trimmed; blank values are ignored so the TOML stays the fallback.
fn apply_nsec_env_override(settings: &mut Settings) {
    if let Some(nsec) = read_nsec_env_var() {
        settings.nostr.nsec_privkey = nsec;
    }
}

/// Validates Mostro settings on startup
fn validate_mostro_settings(settings: &Settings) -> Result<(), MostroError> {
    let dev_fee = settings.mostro.dev_fee_percentage;

    // Validate dev_fee_percentage range
    if dev_fee < MIN_DEV_FEE_PERCENTAGE {
        return Err(MostroInternalErr(ServiceError::IOError(format!(
            "dev_fee_percentage ({}) is below minimum ({})",
            dev_fee, MIN_DEV_FEE_PERCENTAGE
        ))));
    }

    if dev_fee > MAX_DEV_FEE_PERCENTAGE {
        return Err(MostroInternalErr(ServiceError::IOError(format!(
            "dev_fee_percentage ({}) exceeds maximum ({})",
            dev_fee, MAX_DEV_FEE_PERCENTAGE
        ))));
    }

    validate_cashu_settings(
        settings.cashu.as_ref(),
        settings
            .anti_abuse_bond
            .as_ref()
            .is_some_and(|bond| bond.enabled),
    )?;

    validate_rpc_settings(&settings.rpc, read_rpc_token_env_var().as_ref())?;

    Ok(())
}

/// True when `addr` can only be reached from the host itself.
///
/// Parses through `listen_socket_addr`, the same function `RpcServer::bind`
/// uses, so this can never call an address loopback that the server would then
/// refuse to bind. Callers must reject unparseable addresses
/// first — `false` here means "not loopback", not "not an address".
fn is_loopback_address(addr: &str) -> bool {
    listen_socket_addr(addr, 0).is_ok_and(|socket| socket.ip().is_loopback())
}

/// Validate the `[rpc]` block (finding 1.5, issue #807).
///
/// The admin gRPC surface settles disputes, moves escrowed funds, and grants
/// permanent solver rights. Worse, every RPC is executed under the daemon's own
/// identity, which downstream authorization treats as fully privileged
/// (`db::ensure_dispute_finalize_permission`). Reaching the port therefore *is*
/// the authorization, so both guards below are startup-fatal rather than
/// warnings:
///
/// - `enabled = true` requires `MOSTRO_RPC_TOKEN`, so the interceptor always
///   has a credential to check. A daemon that boots without one would serve an
///   admin API that nothing gates.
/// - A non-loopback `listen_address` requires an explicit `allow_remote = true`.
///   The defaults are safe, but nothing used to stop `0.0.0.0` from publishing
///   the admin API to the LAN silently.
/// - `listen_address` must be an address `RpcServer::bind` can actually bind.
///   Validation and binding share `listen_socket_addr` so the two cannot
///   drift: a config accepted here is one the server will accept.
///
/// A half-configured TLS pair is also fatal: it reads as "TLS is on" while
/// serving plaintext.
fn validate_rpc_settings(
    rpc: &RpcSettings,
    token: Option<&SecretString>,
) -> Result<(), MostroError> {
    if !rpc.enabled {
        return Ok(());
    }

    match token {
        None => {
            return Err(MostroInternalErr(ServiceError::IOError(format!(
                "[rpc].enabled = true but {RPC_TOKEN_ENV_VAR} is not set: the admin RPC would \
                 accept every caller that can reach the port. Set {RPC_TOKEN_ENV_VAR} in the \
                 environment or <settings_dir>/.env, or set [rpc].enabled = false."
            ))));
        }
        Some(token) if token.expose_secret().chars().count() < MIN_RPC_TOKEN_LEN => {
            return Err(MostroInternalErr(ServiceError::IOError(format!(
                "{RPC_TOKEN_ENV_VAR} is shorter than {MIN_RPC_TOKEN_LEN} characters: generate a \
                 high-entropy token, e.g. `openssl rand -base64 32`."
            ))));
        }
        // The token travels verbatim inside an HTTP/2 `authorization` header.
        // Anything outside printable ASCII cannot be carried there, so a daemon
        // that accepted it would boot and then refuse every client — a failure
        // that looks like a broken build rather than a typo in the token.
        Some(token) if !token.expose_secret().chars().all(|c| c.is_ascii_graphic()) => {
            return Err(MostroInternalErr(ServiceError::IOError(format!(
                "{RPC_TOKEN_ENV_VAR} must contain only printable ASCII characters and no spaces: \
                 it is sent as an HTTP header, so any other value can never authenticate a \
                 client. `openssl rand -base64 32` produces a valid token."
            ))));
        }
        // The decision not to rate-limit authentication (`rpc::auth`) leans on
        // the token being randomly generated, and length alone does not make
        // it so: `"a".repeat(32)` clears the length gate with almost no
        // entropy. A floor on distinct characters rejects hand-typed values
        // while passing every randomly generated token.
        Some(token)
            if token
                .expose_secret()
                .chars()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                < MIN_RPC_TOKEN_DISTINCT_CHARS =>
        {
            return Err(MostroInternalErr(ServiceError::IOError(format!(
                "{RPC_TOKEN_ENV_VAR} has fewer than {MIN_RPC_TOKEN_DISTINCT_CHARS} distinct \
                 characters: it looks hand-typed rather than randomly generated, and the admin \
                 RPC relies on token entropy instead of a guess counter. Generate it, e.g. \
                 `openssl rand -base64 32`."
            ))));
        }
        Some(_) => {}
    }

    // Before the loopback check, or an unbindable address would be reported as
    // a remote-exposure problem: `localhost` and bare `::1` read as loopback to
    // an operator, so "set allow_remote = true" would be actively misleading
    // advice for a daemon that is about to die on `Invalid address` instead.
    listen_socket_addr(&rpc.listen_address, rpc.port).map_err(|e| {
        MostroInternalErr(ServiceError::IOError(format!("[rpc].listen_address: {e}")))
    })?;

    if rpc.port == 0 {
        return Err(MostroInternalErr(ServiceError::IOError(
            "[rpc].port = 0 asks the kernel for an ephemeral port, which changes on every \
             restart and no client can be configured against. Set a fixed port."
                .to_string(),
        )));
    }

    if !is_loopback_address(&rpc.listen_address) && !rpc.allow_remote {
        return Err(MostroInternalErr(ServiceError::IOError(format!(
            "[rpc].listen_address ({:?}) is not a loopback address: this publishes the admin API \
             beyond this host. Set [rpc].allow_remote = true to confirm this is intended, or bind \
             127.0.0.1.",
            rpc.listen_address
        ))));
    }

    match (rpc.tls_cert_path.as_deref(), rpc.tls_key_path.as_deref()) {
        (Some(_), None) => {
            return Err(MostroInternalErr(ServiceError::IOError(
                "[rpc].tls_cert_path is set without [rpc].tls_key_path: TLS needs both, and the \
                 server would otherwise fall back to plaintext."
                    .to_string(),
            )));
        }
        (None, Some(_)) => {
            return Err(MostroInternalErr(ServiceError::IOError(
                "[rpc].tls_key_path is set without [rpc].tls_cert_path: TLS needs both, and the \
                 server would otherwise fall back to plaintext."
                    .to_string(),
            )));
        }
        (Some(cert), Some(key)) => {
            for (field, path) in [("tls_cert_path", cert), ("tls_key_path", key)] {
                // Open rather than stat: `fs::metadata` succeeds for a file the
                // daemon has no permission to read. Opening is still not
                // enough on its own — on Linux a directory opens fine and only
                // fails on read — so the file type is checked through the
                // handle, which is the capability `RpcServer::start` needs.
                let opened = fs::File::open(path).map_err(|e| {
                    MostroInternalErr(ServiceError::IOError(format!(
                        "[rpc].{field} ({path:?}) is not readable: {e}"
                    )))
                })?;
                let is_regular_file = opened
                    .metadata()
                    .map(|metadata| metadata.is_file())
                    .map_err(|e| {
                        MostroInternalErr(ServiceError::IOError(format!(
                            "[rpc].{field} ({path:?}) could not be inspected: {e}"
                        )))
                    })?;
                if !is_regular_file {
                    return Err(MostroInternalErr(ServiceError::IOError(format!(
                        "[rpc].{field} ({path:?}) is not a regular file"
                    ))));
                }
            }
        }
        (None, None) => {
            if !is_loopback_address(&rpc.listen_address) {
                tracing::warn!(
                    "[rpc] is bound to {} without TLS: admin bearer tokens and dispute data cross \
                     the network in cleartext. Set [rpc].tls_cert_path and [rpc].tls_key_path, or \
                     terminate TLS in a reverse proxy.",
                    rpc.listen_address
                );
            }
        }
    }

    Ok(())
}

/// Validate the `[cashu]` block (Cashu foundation CF-1,
/// `docs/cashu/01-fundamentals.md` §6). Standalone so it is unit-testable
/// without building a full `Settings`.
///
/// Rules (all startup-fatal, so the daemon refuses to boot rather than
/// silently misbehave):
/// - `cashu.enabled` and `anti_abuse_bond.enabled` are mutually exclusive
///   (locked decision §4.5).
/// - When enabled, `mint_url` must be non-empty and parse as `http`/`https`.
/// - When enabled, `escrow_locktime_days >= 1` (the seller-recovery
///   locktime floor of Track A §4B cannot be zero).
fn validate_cashu_settings(
    cashu: Option<&crate::config::types::CashuSettings>,
    bond_enabled: bool,
) -> Result<(), MostroError> {
    let Some(cashu) = cashu else {
        return Ok(());
    };
    if !cashu.enabled {
        return Ok(());
    }

    if bond_enabled {
        return Err(MostroInternalErr(ServiceError::IOError(
            "cashu.enabled and anti_abuse_bond.enabled are mutually exclusive: \
             a node runs bonds or Cashu escrow, never both"
                .to_string(),
        )));
    }

    let url = reqwest::Url::parse(&cashu.mint_url).map_err(|e| {
        MostroInternalErr(ServiceError::IOError(format!(
            "cashu.mint_url ({:?}) is not a valid URL: {e}",
            cashu.mint_url
        )))
    })?;
    if !crate::util::is_http_or_https(&url) {
        return Err(MostroInternalErr(ServiceError::IOError(format!(
            "cashu.mint_url must use http or https, got scheme {:?}",
            url.scheme()
        ))));
    }

    if cashu.escrow_locktime_days < 1 {
        return Err(MostroInternalErr(ServiceError::IOError(format!(
            "cashu.escrow_locktime_days ({}) must be >= 1",
            cashu.escrow_locktime_days
        ))));
    }

    Ok(())
}

/// Initialize the default settings directory and create a settings file from the template if it doesn't exist.
/// Checks if the directory already exists, and if not, creates it and writes the template file.
/// If a custom config path is provided, it uses that instead of the default `~/.mostro` directory.
pub fn init_configuration_file(config_path: Option<String>) -> Result<(), MostroError> {
    let settings_dir = if let Some(user_path) = config_path {
        PathBuf::from(user_path)
    } else {
        let home_dir = dirs::home_dir().ok_or_else(|| {
            MostroInternalErr(ServiceError::IOError(
                "Could not find home directory".to_string(),
            ))
        })?;
        let package_name = env!("CARGO_PKG_NAME");
        home_dir.join(format!(".{}", package_name))
    };

    // Check if /.mostro directory exists
    if !settings_dir.exists() {
        std::fs::create_dir_all(&settings_dir)
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;
    }

    // Load `<settings_dir>/.env` so MOSTRO_NSEC_PRIVKEY (and any future env
    // overrides) can be read from it. Real env vars keep precedence.
    load_env_file(&settings_dir);

    let config_file_path = settings_dir.join("settings.toml");

    if !config_file_path.exists() {
        let mut settings = if std::io::stdin().is_terminal() {
            // Interactive: show setup menu (wizard or manual template)
            wizard::run_setup_menu(&settings_dir, &config_file_path)?
        } else {
            // Non-interactive (Docker, CI, systemd): copy template and exit
            std::fs::write(&config_file_path, include_bytes!("../../settings.tpl.toml"))
                .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;
            println!(
                "Created settings file from template at {} - Edit it to configure your Mostro instance",
                config_file_path.display()
            );
            std::process::exit(0);
        };

        apply_nsec_env_override(&mut settings);
        validate_mostro_settings(&settings)?;
        init_mostro_settings(settings)?;
        tracing::info!("Settings correctly loaded!");
        return Ok(());
    }

    // Read the file content into a zeroizing buffer so TOML plaintext is wiped
    // after parsing.
    let contents = Zeroizing::new(
        fs::read_to_string(&config_file_path)
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?,
    );

    // Parse TOML content
    let mut settings: Settings = toml::from_str(&contents)
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

    // Apply MOSTRO_NSEC_PRIVKEY override before validation so an empty TOML
    // value is fine when the env var is set.
    apply_nsec_env_override(&mut settings);

    // Validate settings before initializing
    validate_mostro_settings(&settings)?;

    // Override database URL
    settings.database.url = format!("sqlite://{}", settings_dir.join(DB_FILENAME).display());

    // Initialize the global settings variable
    init_mostro_settings(settings)?;

    tracing::info!("Settings correctly loaded!");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::constants::NSEC_ENV_VAR;
    use crate::config::types::{
        DatabaseSettings, LightningSettings, MostroSettings, NostrSettings, RpcSettings,
    };
    use secrecy::{ExposeSecret, SecretString};

    /// RAII guard that saves the current value of an env var and restores it
    /// on drop, so tests don't leak state into each other.
    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn new(key: &'static str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, previous }
        }

        fn set(&self, value: &str) {
            std::env::set_var(self.key, value);
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(val) => std::env::set_var(self.key, val),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn make_settings(nsec: &str) -> Settings {
        Settings {
            database: DatabaseSettings::default(),
            lightning: LightningSettings::default(),
            nostr: NostrSettings {
                nsec_privkey: SecretString::from(nsec.to_owned()),
                relays: vec!["wss://relay.test".to_string()],
            },
            mostro: MostroSettings::default(),
            rpc: RpcSettings::default(),
            expiration: None,
            anti_abuse_bond: None,
            cashu: None,
            price: None,
        }
    }

    #[test]
    fn env_var_overrides_toml_nsec() {
        let _lock = ENV_LOCK.blocking_lock();
        let guard = EnvVarGuard::new(NSEC_ENV_VAR);
        guard.set("nsec_from_env");

        let mut settings = make_settings("nsec_from_toml");
        apply_nsec_env_override(&mut settings);

        assert_eq!(settings.nostr.nsec_privkey.expose_secret(), "nsec_from_env");
    }

    #[test]
    fn empty_env_var_falls_back_to_toml() {
        let _lock = ENV_LOCK.blocking_lock();
        let guard = EnvVarGuard::new(NSEC_ENV_VAR);
        guard.set("");

        let mut settings = make_settings("nsec_from_toml");
        apply_nsec_env_override(&mut settings);

        assert_eq!(
            settings.nostr.nsec_privkey.expose_secret(),
            "nsec_from_toml"
        );
    }

    #[test]
    fn no_env_var_keeps_toml() {
        let _lock = ENV_LOCK.blocking_lock();
        let _guard = EnvVarGuard::new(NSEC_ENV_VAR);

        let mut settings = make_settings("nsec_from_toml");
        apply_nsec_env_override(&mut settings);

        assert_eq!(
            settings.nostr.nsec_privkey.expose_secret(),
            "nsec_from_toml"
        );
    }

    #[test]
    fn whitespace_only_env_is_ignored() {
        let _lock = ENV_LOCK.blocking_lock();
        let guard = EnvVarGuard::new(NSEC_ENV_VAR);
        guard.set("   \t  ");

        let mut settings = make_settings("nsec_from_toml");
        apply_nsec_env_override(&mut settings);

        assert_eq!(
            settings.nostr.nsec_privkey.expose_secret(),
            "nsec_from_toml"
        );
    }

    #[test]
    fn env_guard_restores_preexisting_value_on_drop() {
        // When the env var already held a value, the guard must restore that
        // exact value on drop (the `Some(previous)` restore arm), not leave
        // the test's override leaking into sibling tests.
        let _lock = ENV_LOCK.blocking_lock();
        std::env::set_var(NSEC_ENV_VAR, "preexisting_value");
        {
            let guard = EnvVarGuard::new(NSEC_ENV_VAR);
            guard.set("temporary_override");
            assert_eq!(
                std::env::var(NSEC_ENV_VAR).as_deref(),
                Ok("temporary_override")
            );
        }
        // Drop restored the original value.
        assert_eq!(
            std::env::var(NSEC_ENV_VAR).as_deref(),
            Ok("preexisting_value")
        );
        std::env::remove_var(NSEC_ENV_VAR);
    }

    #[test]
    fn env_var_value_is_trimmed() {
        let _lock = ENV_LOCK.blocking_lock();
        let guard = EnvVarGuard::new(NSEC_ENV_VAR);
        guard.set("  nsec_from_env  ");

        let mut settings = make_settings("nsec_from_toml");
        apply_nsec_env_override(&mut settings);

        assert_eq!(settings.nostr.nsec_privkey.expose_secret(), "nsec_from_env");
    }

    #[test]
    fn toml_parses_without_nsec_privkey_field() {
        // Operators who rely exclusively on MOSTRO_NSEC_PRIVKEY should be able
        // to omit nsec_privkey from settings.toml entirely.
        let toml_without_nsec = r#"relays = ["wss://relay.test"]"#;
        let nostr: NostrSettings =
            toml::from_str(toml_without_nsec).expect("nsec_privkey should be optional in TOML");
        assert!(nostr.nsec_privkey.expose_secret().is_empty());
        assert_eq!(nostr.relays, vec!["wss://relay.test"]);
    }
}

#[cfg(test)]
mod cashu_validation_tests {
    use super::*;
    use crate::config::types::CashuSettings;

    fn enabled(mint_url: &str, days: u32) -> CashuSettings {
        CashuSettings {
            enabled: true,
            mint_url: mint_url.to_string(),
            escrow_locktime_days: days,
        }
    }

    #[test]
    fn absent_block_is_valid_regardless_of_bonds() {
        assert!(validate_cashu_settings(None, false).is_ok());
        assert!(validate_cashu_settings(None, true).is_ok());
    }

    #[test]
    fn disabled_block_is_valid_even_with_bonds() {
        let cashu = CashuSettings::default();
        assert!(validate_cashu_settings(Some(&cashu), true).is_ok());
    }

    #[test]
    fn rejects_cashu_and_bonds_together() {
        // Locked decision §4.5: a node runs bonds or Cashu, never both.
        let cashu = enabled("https://mint.example.com", 15);
        assert!(validate_cashu_settings(Some(&cashu), true).is_err());
    }

    #[test]
    fn accepts_valid_enabled_config() {
        let cashu = enabled("https://mint.example.com", 15);
        assert!(validate_cashu_settings(Some(&cashu), false).is_ok());
        let cashu_http = enabled("http://localhost:3338", 1);
        assert!(validate_cashu_settings(Some(&cashu_http), false).is_ok());
    }

    #[test]
    fn rejects_empty_or_malformed_mint_url() {
        assert!(validate_cashu_settings(Some(&enabled("", 15)), false).is_err());
        assert!(validate_cashu_settings(Some(&enabled("not a url", 15)), false).is_err());
    }

    #[test]
    fn rejects_non_http_scheme() {
        let cashu = enabled("ftp://mint.example.com", 15);
        assert!(validate_cashu_settings(Some(&cashu), false).is_err());
        let cashu_ws = enabled("wss://mint.example.com", 15);
        assert!(validate_cashu_settings(Some(&cashu_ws), false).is_err());
    }

    #[test]
    fn rejects_zero_locktime_days() {
        // Track A §4B: the seller-recovery locktime floor cannot be zero.
        let cashu = enabled("https://mint.example.com", 0);
        assert!(validate_cashu_settings(Some(&cashu), false).is_err());
    }
}

#[cfg(test)]
mod startup_validation_tests {
    use super::*;
    use crate::config::constants::{MAX_DEV_FEE_PERCENTAGE, MIN_DEV_FEE_PERCENTAGE};
    use crate::config::types::{
        AntiAbuseBondSettings, CashuSettings, DatabaseSettings, LightningSettings, MostroSettings,
        NostrSettings, RpcSettings,
    };

    fn base_settings() -> Settings {
        Settings {
            database: DatabaseSettings::default(),
            lightning: LightningSettings::default(),
            nostr: NostrSettings::default(),
            mostro: MostroSettings::default(),
            rpc: RpcSettings::default(),
            expiration: None,
            anti_abuse_bond: None,
            cashu: None,
            price: None,
        }
    }

    #[test]
    fn default_settings_pass_validation() {
        // `validate_mostro_settings` reads MOSTRO_RPC_TOKEN from the
        // environment, so these tests hold the crate-wide lock like every
        // other reader: a concurrent `setenv` elsewhere is a data race.
        let _lock = ENV_LOCK.blocking_lock();
        assert!(validate_mostro_settings(&base_settings()).is_ok());
    }

    #[test]
    fn dev_fee_below_minimum_is_rejected() {
        let _lock = ENV_LOCK.blocking_lock();
        let mut settings = base_settings();
        settings.mostro.dev_fee_percentage = MIN_DEV_FEE_PERCENTAGE - 0.01;
        let err = validate_mostro_settings(&settings).expect_err("below-min dev fee must fail");
        assert!(err.to_string().contains("below minimum"));
    }

    #[test]
    fn dev_fee_above_maximum_is_rejected() {
        let _lock = ENV_LOCK.blocking_lock();
        let mut settings = base_settings();
        settings.mostro.dev_fee_percentage = MAX_DEV_FEE_PERCENTAGE + 0.01;
        let err = validate_mostro_settings(&settings).expect_err("above-max dev fee must fail");
        assert!(err.to_string().contains("exceeds maximum"));
    }

    #[test]
    fn cashu_and_bond_conflict_is_rejected_through_full_validation() {
        let _lock = ENV_LOCK.blocking_lock();
        let mut settings = base_settings();
        settings.anti_abuse_bond = Some(AntiAbuseBondSettings {
            enabled: true,
            ..Default::default()
        });
        settings.cashu = Some(CashuSettings {
            enabled: true,
            mint_url: "https://mint.example.com".to_string(),
            escrow_locktime_days: 15,
        });
        assert!(validate_mostro_settings(&settings).is_err());
    }
}

#[cfg(test)]
mod rpc_validation_tests {
    use super::*;
    use crate::config::types::RpcSettings;

    fn valid_token() -> SecretString {
        // 32 distinct characters: clears both the length and the entropy
        // floors the validator enforces.
        SecretString::from("0123456789abcdefghijklmnopqrstuv")
    }

    fn enabled_rpc() -> RpcSettings {
        RpcSettings {
            enabled: true,
            ..Default::default()
        }
    }

    fn temp_pem(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!("mostro-rpc-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join(format!("{tag}.pem"));
        std::fs::write(&path, b"not a real certificate").expect("write pem");
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn disabled_rpc_needs_no_token() {
        // The whole block is inert when the server never starts, including a
        // deliberately unsafe bind.
        let rpc = RpcSettings {
            listen_address: "0.0.0.0".to_string(),
            ..Default::default()
        };
        assert!(validate_rpc_settings(&rpc, None).is_ok());
    }

    #[test]
    fn enabled_rpc_without_a_token_is_rejected() {
        let err = validate_rpc_settings(&enabled_rpc(), None)
            .expect_err("an ungated admin RPC must not boot");
        assert!(err.to_string().contains(RPC_TOKEN_ENV_VAR));
    }

    #[test]
    fn enabled_rpc_with_a_short_token_is_rejected() {
        let short = SecretString::from("a".repeat(MIN_RPC_TOKEN_LEN - 1));
        let err = validate_rpc_settings(&enabled_rpc(), Some(&short))
            .expect_err("a guessable token must not boot");
        assert!(err.to_string().contains("shorter than"));
    }

    #[test]
    fn enabled_rpc_on_loopback_with_a_token_is_accepted() {
        assert!(validate_rpc_settings(&enabled_rpc(), Some(&valid_token())).is_ok());
    }

    #[test]
    fn a_token_that_cannot_travel_in_a_header_is_rejected() {
        // Long enough to clear the length gate, but unusable as an HTTP header
        // value: accepting it would boot a daemon that refuses every client.
        for unusable in ["é".repeat(MIN_RPC_TOKEN_LEN), "a".repeat(31) + " b"] {
            let token = SecretString::from(unusable.clone());
            let err = validate_rpc_settings(&enabled_rpc(), Some(&token))
                .expect_err("a token that cannot be sent must not boot");
            assert!(
                err.to_string().contains("printable ASCII"),
                "{unusable:?} should have been refused as unsendable, got: {err}"
            );
        }
    }

    #[test]
    fn a_low_entropy_token_is_rejected() {
        // Long enough and ASCII, but visibly hand-typed: the length gate
        // alone would accept both and quietly void the no-rate-limit
        // argument in `rpc::auth`.
        for guessable in ["a".repeat(MIN_RPC_TOKEN_LEN), "abcdefg1".repeat(4)] {
            let token = SecretString::from(guessable.clone());
            let err = validate_rpc_settings(&enabled_rpc(), Some(&token))
                .expect_err("a low-entropy token must not boot");
            assert!(
                err.to_string().contains("distinct"),
                "{guessable:?} should have been refused as low-entropy, got: {err}"
            );
        }
    }

    #[test]
    fn port_zero_is_rejected() {
        let rpc = RpcSettings {
            port: 0,
            ..enabled_rpc()
        };
        let err = validate_rpc_settings(&rpc, Some(&valid_token()))
            .expect_err("an ephemeral admin port must not boot");
        assert!(err.to_string().contains("ephemeral"));
    }

    #[test]
    fn listen_socket_addr_accepts_only_bindable_literals() {
        for address in ["127.0.0.1", "[::1]", "0.0.0.0", "[::]"] {
            assert!(
                listen_socket_addr(address, 50051).is_ok(),
                "{address} is a bindable literal"
            );
        }
        for address in ["localhost", "::1", "::", "mostro.example.com", ""] {
            assert!(
                listen_socket_addr(address, 50051).is_err(),
                "{address} is not a bindable literal"
            );
        }
    }

    #[test]
    fn a_directory_is_not_accepted_as_tls_material() {
        // Both `fs::metadata` and `File::open` succeed on a directory, so only
        // the file-type check rejects this.
        let dir = std::env::temp_dir().join(format!("mostro-rpc-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let rpc = RpcSettings {
            tls_cert_path: Some(dir.to_string_lossy().into_owned()),
            tls_key_path: Some(temp_pem("dir-case-key")),
            ..enabled_rpc()
        };
        let err = validate_rpc_settings(&rpc, Some(&valid_token()))
            .expect_err("a directory is not a certificate");
        assert!(err.to_string().contains("not a regular file"));
    }

    #[test]
    fn loopback_is_recognised_in_every_bindable_form() {
        for address in ["127.0.0.1", "127.0.0.53", "[::1]"] {
            let rpc = RpcSettings {
                enabled: true,
                listen_address: address.to_string(),
                ..Default::default()
            };
            assert!(
                validate_rpc_settings(&rpc, Some(&valid_token())).is_ok(),
                "{address} should be treated as loopback"
            );
        }
    }

    /// The contract this pins: validation and `RpcServer::bind` share one
    /// parser, so anything the server cannot bind is refused here with an
    /// actionable message instead of at startup with `Invalid address`.
    ///
    /// `localhost` and bare `::1` are the cases that matter — they look like
    /// valid loopback spellings, and reporting them through the `allow_remote`
    /// branch would send the operator to fix the wrong setting.
    #[test]
    fn an_unbindable_listen_address_is_rejected() {
        for address in ["localhost", "LOCALHOST", "::1", "::", "mostro.example.com"] {
            let rpc = RpcSettings {
                enabled: true,
                listen_address: address.to_string(),
                // Set so the failure cannot be attributed to the remote-bind
                // guard: only the parse check can refuse these.
                allow_remote: true,
                ..Default::default()
            };
            let err = validate_rpc_settings(&rpc, Some(&valid_token()))
                .expect_err("an address the server cannot bind must not boot");
            let message = err.to_string();
            assert!(
                message.contains("IP literal") && message.contains("[::1]"),
                "{address} should name the accepted spellings, got: {message}"
            );
        }
    }

    #[test]
    fn non_loopback_bind_without_allow_remote_is_rejected() {
        for address in ["0.0.0.0", "192.168.1.10", "[::]"] {
            let rpc = RpcSettings {
                enabled: true,
                listen_address: address.to_string(),
                ..Default::default()
            };
            let err = validate_rpc_settings(&rpc, Some(&valid_token()))
                .expect_err("a routable bind must require allow_remote");
            assert!(
                err.to_string().contains("allow_remote"),
                "{address} should have been refused, got: {err}"
            );
        }
    }

    #[test]
    fn non_loopback_bind_with_allow_remote_is_accepted() {
        let rpc = RpcSettings {
            enabled: true,
            listen_address: "0.0.0.0".to_string(),
            allow_remote: true,
            ..Default::default()
        };
        assert!(validate_rpc_settings(&rpc, Some(&valid_token())).is_ok());
    }

    #[test]
    fn half_configured_tls_is_rejected() {
        let cert_only = RpcSettings {
            tls_cert_path: Some(temp_pem("cert-only")),
            ..enabled_rpc()
        };
        assert!(validate_rpc_settings(&cert_only, Some(&valid_token()))
            .expect_err("cert without key must fail")
            .to_string()
            .contains("tls_key_path"));

        let key_only = RpcSettings {
            tls_key_path: Some(temp_pem("key-only")),
            ..enabled_rpc()
        };
        assert!(validate_rpc_settings(&key_only, Some(&valid_token()))
            .expect_err("key without cert must fail")
            .to_string()
            .contains("tls_cert_path"));
    }

    #[test]
    fn unreadable_tls_material_is_rejected() {
        let rpc = RpcSettings {
            tls_cert_path: Some("/nonexistent/mostro-rpc.pem".to_string()),
            tls_key_path: Some(temp_pem("readable-key")),
            ..enabled_rpc()
        };
        let err = validate_rpc_settings(&rpc, Some(&valid_token()))
            .expect_err("unreadable TLS material must fail");
        assert!(err.to_string().contains("not readable"));
    }

    #[test]
    fn readable_tls_pair_is_accepted() {
        let rpc = RpcSettings {
            tls_cert_path: Some(temp_pem("pair-cert")),
            tls_key_path: Some(temp_pem("pair-key")),
            ..enabled_rpc()
        };
        assert!(validate_rpc_settings(&rpc, Some(&valid_token())).is_ok());
    }

    #[test]
    fn tls_paths_helper_requires_both_halves() {
        let rpc = RpcSettings {
            tls_cert_path: Some("cert.pem".to_string()),
            ..Default::default()
        };
        assert!(rpc.tls_paths().is_none());

        let rpc = RpcSettings {
            tls_cert_path: Some("cert.pem".to_string()),
            tls_key_path: Some("key.pem".to_string()),
            ..Default::default()
        };
        assert_eq!(rpc.tls_paths(), Some(("cert.pem", "key.pem")));
    }
}

#[cfg(test)]
mod env_file_tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mostro-config-util-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn missing_env_file_is_a_noop() {
        let dir = temp_dir("no-env");
        // Must not error or panic when `<dir>/.env` is absent.
        load_env_file(&dir);
    }

    #[test]
    fn env_file_values_become_process_env() {
        // dotenvy mutates the environment, so the crate-wide lock applies
        // even though the variable name is unique to this test.
        let _lock = ENV_LOCK.blocking_lock();
        let dir = temp_dir("with-env");
        std::fs::write(
            dir.join(ENV_FILENAME),
            "MOSTRO_TEST_ENV_FILE_MARKER=loaded\n",
        )
        .expect("write .env");
        load_env_file(&dir);
        assert_eq!(
            std::env::var("MOSTRO_TEST_ENV_FILE_MARKER").as_deref(),
            Ok("loaded")
        );
    }

    #[test]
    fn unreadable_env_file_logs_and_continues() {
        let dir = temp_dir("bad-env");
        // A directory named `.env` makes dotenvy fail; the loader must warn
        // and fall back instead of propagating the error.
        std::fs::create_dir_all(dir.join(ENV_FILENAME)).expect("create .env dir");
        load_env_file(&dir);
    }
}

#[cfg(test)]
mod init_configuration_file_tests {
    use super::*;

    fn temp_config_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mostro-init-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    // NOTE: the success path (valid settings.toml) calls
    // `init_mostro_settings`, which panics when the global OnceLock is
    // already set by another test — and the missing-file path calls
    // `std::process::exit(0)` when stdin is not a terminal, which would
    // kill the whole test binary. Only the error paths are testable here.

    #[test]
    fn malformed_toml_is_rejected() {
        let dir = temp_config_dir("bad-toml");
        std::fs::write(dir.join("settings.toml"), "this is not = [valid toml")
            .expect("write settings.toml");
        let result = init_configuration_file(Some(dir.to_string_lossy().into_owned()));
        assert!(result.is_err());
    }

    #[test]
    fn structurally_valid_toml_with_bad_dev_fee_is_rejected() {
        let dir = temp_config_dir("bad-dev-fee");
        // Start from the shipped template so the TOML parses, then push the
        // dev fee out of range so validation (not parsing) rejects it.
        let template = std::str::from_utf8(include_bytes!("../../settings.tpl.toml"))
            .expect("template is UTF-8");
        let tampered =
            template.replace("dev_fee_percentage = ", "dev_fee_percentage = 99.0 # was: ");
        assert!(
            tampered.contains("99.0"),
            "template must contain dev_fee_percentage for this test to be meaningful"
        );
        std::fs::write(dir.join("settings.toml"), tampered).expect("write settings.toml");
        let result = init_configuration_file(Some(dir.to_string_lossy().into_owned()));
        assert!(result.is_err());
    }
}
