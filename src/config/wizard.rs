use std::path::{Path, PathBuf};

use dialoguer::{Confirm, Input, Password, Select};
use mostro_core::error::MostroError::{self, MostroInternalErr};
use mostro_core::error::ServiceError;
use nostr_sdk::prelude::*;
use secrecy::{ExposeSecret, SecretString};
use zeroize::Zeroizing;

use super::constants::{DB_FILENAME, ENV_FILENAME, NSEC_ENV_VAR};
use super::permissions::{create_owner_only, write_owner_only_atomic};
use super::settings::Settings;
use super::types::{
    DatabaseSettings, LightningSettings, MostroSettings, NostrSettings, RpcSettings,
};

const TEMPLATE_BYTES: &[u8] = include_bytes!("../../settings.tpl.toml");

/// Show the initial setup menu and return a configured Settings if the user
/// chose the interactive wizard. If manual setup is chosen, the template is
/// written and the process exits.
pub fn run_setup_menu(
    settings_dir: &Path,
    config_file_path: &Path,
) -> Result<Settings, MostroError> {
    println!("\nWelcome to Mostro! No configuration found.\n");

    let choices = &[
        "Interactive setup (guided wizard)",
        "Manual setup (creates settings.toml template for you to edit)",
    ];

    let selection = Select::new()
        .with_prompt("How would you like to set up your instance?")
        .items(choices)
        .default(0)
        .interact()
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

    match selection {
        0 => {
            let settings = run_setup_wizard(settings_dir, config_file_path)?;
            Ok(settings)
        }
        _ => {
            create_owner_only(config_file_path, TEMPLATE_BYTES)?;
            println!(
                "Created settings file from template at {} - Edit it to configure your Mostro instance",
                config_file_path.display()
            );
            std::process::exit(0);
        }
    }
}

fn run_setup_wizard(settings_dir: &Path, config_file_path: &Path) -> Result<Settings, MostroError> {
    println!("\n--- Lightning (LND) Configuration ---\n");

    let lightning = prompt_lightning_settings()?;

    println!("\n--- Nostr Configuration ---\n");

    let nostr = prompt_nostr_settings(settings_dir)?;

    println!("\n--- Mostro Configuration ---\n");

    let mostro = prompt_mostro_settings()?;

    let settings = Settings {
        database: DatabaseSettings::default(),
        lightning,
        nostr,
        mostro,
        rpc: RpcSettings::default(),
        expiration: None,
        anti_abuse_bond: None,
        cashu: None,
        price: None,
    };

    save_settings(config_file_path, &settings)?;

    println!("\nConfiguration saved to {}\n", config_file_path.display());

    // Override database URL to use settings directory
    let mut settings = settings;
    settings.database.url = format!("sqlite://{}", settings_dir.join(DB_FILENAME).display());

    Ok(settings)
}

fn prompt_lightning_settings() -> Result<LightningSettings, MostroError> {
    let lnd_cert_file: String = Input::new()
        .with_prompt("Path to LND tls.cert file")
        .validate_with(|input: &String| validate_file_exists(input))
        .interact_text()
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;
    let lnd_cert_file = resolve_file_path(&lnd_cert_file)?;

    let lnd_macaroon_file: String = Input::new()
        .with_prompt("Path to LND admin.macaroon file")
        .validate_with(|input: &String| validate_file_exists(input))
        .interact_text()
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;
    let lnd_macaroon_file = resolve_file_path(&lnd_macaroon_file)?;

    let lnd_grpc_host: String = Input::new()
        .with_prompt("LND gRPC host")
        .default("https://127.0.0.1:10009".to_string())
        .interact_text()
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

    Ok(LightningSettings {
        lnd_cert_file,
        lnd_macaroon_file,
        lnd_grpc_host,
        invoice_expiration_window: 3600,
        hold_invoice_cltv_delta: 144,
        hold_invoice_expiration_window: 300,
        payment_attempts: 3,
        payment_retries_interval: 60,
        max_final_cltv_expiry_delta: 144,
        escrow_deadline_margin_blocks: 24,
        max_inflight_payouts: 100,
        max_inflight_payouts_per_destination: 10,
        payment_cltv_limit: 1008,
        allow_node_change: false,
    })
}

fn prompt_nostr_settings(settings_dir: &Path) -> Result<NostrSettings, MostroError> {
    let has_nsec = Confirm::new()
        .with_prompt("Do you have an existing nsec key?")
        .default(false)
        .interact()
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

    let nsec = if has_nsec {
        let input = Zeroizing::new(
            Password::new()
                .with_prompt("Enter your nsec private key")
                .validate_with(|input: &String| validate_nsec(input))
                .interact()
                .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?,
        );
        SecretString::from(input.to_string())
    } else {
        let keys = Keys::generate();
        let nsec = keys
            .secret_key()
            .to_bech32()
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;
        let npub = keys
            .public_key()
            .to_bech32()
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

        println!("\nGenerated new Nostr keypair:");
        println!("  npub: {npub}");
        println!("  You will be prompted to store the private key securely next.");

        SecretString::from(nsec)
    };

    let nsec_privkey = prompt_nsec_storage(settings_dir, &nsec)?;

    let relays_input: String = Input::new()
        .with_prompt("Nostr relays (comma-separated)")
        .default("wss://relay.mostro.network".to_string())
        .validate_with(|input: &String| validate_relays(input))
        .interact_text()
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

    let relays: Vec<String> = relays_input
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    Ok(NostrSettings {
        nsec_privkey,
        relays,
    })
}

/// Ask the user where to persist the nsec and return the value that should be
/// written into `settings.toml` (empty string when the key is stored elsewhere).
fn prompt_nsec_storage(
    settings_dir: &Path,
    nsec: &SecretString,
) -> Result<SecretString, MostroError> {
    println!("\nMostro supports two storage locations for your nsec. Both are fully supported;");
    println!("pick the one that fits your threat model and deployment setup. You can also");
    println!("provide MOSTRO_NSEC_PRIVKEY via the real process environment (systemd, Docker,");
    println!("shell, secrets manager) instead — in that case either option below works as a");
    println!("starting point and you can move the key elsewhere afterwards.\n");

    let env_file_path = settings_dir.join(ENV_FILENAME);
    let choices = &[
        "Save to .env (auto-loaded at startup, chmod 600)",
        "Save inline in settings.toml",
    ];

    let selection = Select::new()
        .with_prompt("Where do you want to store your Nostr private key?")
        .items(choices)
        .default(0)
        .interact()
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

    let nsec_in_toml = if selection == 0 {
        write_env_file(&env_file_path, nsec.expose_secret())?;
        // Export the key into the current process so the daemon can use it
        // immediately after the wizard finishes, without requiring a restart.
        std::env::set_var(NSEC_ENV_VAR, nsec.expose_secret());
        println!(
            "\n  Private key saved to {} (permissions 600).",
            env_file_path.display()
        );
        SecretString::default()
    } else {
        println!(
            "\n  Private key will be written inside {}.",
            settings_dir.join("settings.toml").display()
        );
        nsec.clone()
    };

    println!(
        "\n  IMPORTANT: Back up your nsec in a secure place. If you lose it, you lose control of this Mostro instance's identity.\n"
    );

    Ok(nsec_in_toml)
}

/// Serialize `settings` and create `config_file_path` from it, owner-only.
///
/// Shares `create_owner_only` with the manual template copy here and the
/// non-interactive one in `config::util`, so every path that brings an initial
/// `settings.toml` into existence gets the same mode and the same refusal to
/// follow a symlink — this file is about to hold `nsec_privkey`.
fn save_settings(config_file_path: &Path, settings: &Settings) -> Result<(), MostroError> {
    let toml_content = Zeroizing::new(
        toml::to_string_pretty(settings)
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?,
    );
    create_owner_only(config_file_path, toml_content.as_bytes())
}

/// Write `MOSTRO_NSEC_PRIVKEY=<nsec>` to the given path, owner-only (`0600` on
/// Unix), replacing whatever is already there.
///
/// Goes through `write_owner_only_atomic` rather than opening the path
/// directly. `.env` holds the same `nsec_privkey` as `settings.toml` and, in
/// the wizard flow, is written first — so opening it with
/// `create(true).truncate(true)` would follow a symlink another local account
/// planted in the settings directory, truncating its target and resetting it
/// to `0600`. That is the one thing `create_owner_only` exists to prevent for
/// `settings.toml`, and this file is worth exactly as much. `create_owner_only`
/// itself cannot serve here: it refuses a path that already exists, and
/// rewriting an existing `.env` is a supported thing to do.
///
/// The line goes through a `Zeroizing` buffer so the plaintext nsec is wiped
/// once handed off, the way `save_settings` treats the serialized TOML.
fn write_env_file(path: &Path, nsec: &str) -> Result<(), MostroError> {
    let line = Zeroizing::new(format!("{}={}\n", NSEC_ENV_VAR, nsec));
    write_owner_only_atomic(path, line.as_bytes())
}

fn prompt_mostro_settings() -> Result<MostroSettings, MostroError> {
    let fee: f64 = Input::new()
        .with_prompt("Mostro fee (e.g. 0.01 = 1%)")
        .default(0.0)
        .interact_text()
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

    let fiat_input: String = Input::new()
        .with_prompt("Fiat currencies accepted (comma-separated, empty = all)")
        .default(String::new())
        .show_default(false)
        .interact_text()
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

    let fiat_currencies_accepted: Vec<String> = if fiat_input.trim().is_empty() {
        vec![]
    } else {
        fiat_input
            .split(',')
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty())
            .collect()
    };

    Ok(MostroSettings {
        fee,
        fiat_currencies_accepted,
        ..MostroSettings::default()
    })
}

// --- Validation helpers ---

pub fn validate_file_exists(path: &str) -> Result<(), String> {
    let expanded = expand_tilde(path);
    if !expanded.exists() {
        return Err(format!("File not found: {}", expanded.display()));
    }
    if !expanded.is_file() {
        return Err(format!(
            "Path is not a regular file: {}",
            expanded.display()
        ));
    }
    Ok(())
}

pub fn resolve_file_path(path: &str) -> Result<String, MostroError> {
    let expanded = expand_tilde(path);
    std::fs::canonicalize(&expanded)
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))
}

pub fn validate_nsec(input: &str) -> Result<(), String> {
    Keys::parse(input.trim())
        .map(|_| ())
        .map_err(|e| format!("Invalid nsec key: {}", e))
}

pub fn validate_relays(input: &str) -> Result<(), String> {
    let relays: Vec<&str> = input
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if relays.is_empty() {
        return Err("At least one relay is required".to_string());
    }
    for relay in &relays {
        if !relay.starts_with("ws://") && !relay.starts_with("wss://") {
            return Err(format!(
                "Invalid relay URL (must start with ws:// or wss://): {}",
                relay
            ));
        }
    }
    Ok(())
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::{assert_mode, set_mode};

    #[test]
    fn test_validate_nsec_valid() {
        assert!(
            validate_nsec("nsec13as48eum93hkg7plv526r9gjpa0uc52zysqm93pmnkca9e69x6tsdjmdxd")
                .is_ok()
        );
    }

    #[test]
    fn test_validate_nsec_invalid() {
        assert!(validate_nsec("not_a_valid_nsec").is_err());
        assert!(validate_nsec("").is_err());
    }

    #[test]
    fn test_validate_relays_valid() {
        assert!(validate_relays("wss://relay.mostro.network").is_ok());
        assert!(validate_relays("wss://relay1.com, wss://relay2.com").is_ok());
        assert!(validate_relays("ws://localhost:7000").is_ok());
    }

    #[test]
    fn test_validate_relays_invalid() {
        assert!(validate_relays("").is_err());
        assert!(validate_relays("http://not-a-relay.com").is_err());
        assert!(validate_relays("wss://good.com, http://bad.com").is_err());
    }

    #[test]
    fn test_validate_file_exists_nonexistent() {
        assert!(validate_file_exists("/nonexistent/path/to/file.cert").is_err());
    }

    #[test]
    fn test_expand_tilde() {
        let expanded = expand_tilde("~/test");
        assert!(!expanded.to_string_lossy().starts_with("~/"));
    }

    #[test]
    fn test_expand_tilde_no_tilde() {
        let path = "/absolute/path";
        assert_eq!(expand_tilde(path), PathBuf::from(path));
    }

    fn temp_dir(tag: &str) -> PathBuf {
        crate::config::test_support::temp_dir("wizard", tag)
    }

    #[test]
    fn test_validate_file_exists_accepts_regular_file() {
        let dir = temp_dir("file-ok");
        let file = dir.join("tls.cert");
        std::fs::write(&file, b"cert bytes").expect("write file");
        assert!(validate_file_exists(&file.to_string_lossy()).is_ok());
    }

    #[test]
    fn test_validate_file_exists_rejects_directory() {
        let dir = temp_dir("file-dir");
        let err = validate_file_exists(&dir.to_string_lossy())
            .expect_err("a directory is not a regular file");
        assert!(err.contains("not a regular file"));
    }

    #[test]
    fn test_resolve_file_path_canonicalizes_existing_file() {
        let dir = temp_dir("resolve-ok");
        let file = dir.join("admin.macaroon");
        std::fs::write(&file, b"macaroon").expect("write file");
        let resolved = resolve_file_path(&file.to_string_lossy()).expect("existing file resolves");
        assert!(resolved.ends_with("admin.macaroon"));
        assert!(PathBuf::from(resolved).is_absolute());
    }

    #[test]
    fn test_resolve_file_path_errors_on_missing_file() {
        assert!(resolve_file_path("/definitely/not/here.macaroon").is_err());
    }

    #[test]
    fn test_write_env_file_creates_file_with_owner_only_permissions() {
        let dir = temp_dir("env-new");
        let env_path = dir.join(ENV_FILENAME);

        write_env_file(&env_path, "nsec1testvalue").expect("write env file");

        let contents = std::fs::read_to_string(&env_path).expect("read env file");
        assert_eq!(contents, format!("{}=nsec1testvalue\n", NSEC_ENV_VAR));
        assert_mode(&env_path, 0o600);
    }

    #[test]
    fn test_write_env_file_tightens_preexisting_broader_permissions() {
        let dir = temp_dir("env-existing");
        let env_path = dir.join(ENV_FILENAME);
        std::fs::write(&env_path, "OLD=stale\n").expect("seed env file");
        set_mode(&env_path, 0o644);

        write_env_file(&env_path, "nsec1replaced").expect("rewrite env file");

        // Old content replaced, permissions tightened back to 0600.
        let contents = std::fs::read_to_string(&env_path).expect("read env file");
        assert_eq!(contents, format!("{}=nsec1replaced\n", NSEC_ENV_VAR));
        assert_mode(&env_path, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn test_write_env_file_does_not_write_the_nsec_through_a_planted_symlink() {
        let dir = temp_dir("env-symlink");
        let victim = dir.join("victim");
        std::fs::write(&victim, "victim contents").expect("seed victim");
        set_mode(&victim, 0o644);

        // The wizard writes `.env` before `settings.toml`, so on a settings
        // directory another local account can write to this is the first shot
        // it gets at the nsec.
        let env_path = dir.join(ENV_FILENAME);
        std::os::unix::fs::symlink(&victim, &env_path).expect("plant symlink");

        write_env_file(&env_path, "nsec1secret").expect("write env file");

        assert_eq!(
            std::fs::read_to_string(&victim).expect("read victim"),
            "victim contents",
            "the nsec must not be written through the link"
        );
        assert_mode(&victim, 0o644);
        assert_mode(&env_path, 0o600);
    }
}

#[cfg(test)]
mod save_settings_tests {
    use super::*;
    use crate::config::test_support::{assert_mode, set_mode};

    fn temp_root(tag: &str) -> PathBuf {
        crate::config::test_support::temp_dir("wizard-save", tag)
    }

    fn sample_settings() -> Settings {
        Settings {
            database: DatabaseSettings::default(),
            lightning: LightningSettings::default(),
            nostr: NostrSettings {
                nsec_privkey: "nsec13as48eum93hkg7plv526r9gjpa0uc52zysqm93pmnkca9e69x6tsdjmdxd"
                    .to_string()
                    .into(),
                relays: vec!["wss://relay.mostro.network".to_string()],
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
    fn wizard_save_creates_the_file_owner_only() {
        let root = temp_root("ok");
        let config_file = root.join("settings.toml");
        save_settings(&config_file, &sample_settings()).expect("save settings");
        assert_mode(&config_file, 0o600);
        let written = std::fs::read_to_string(&config_file).expect("read back");
        assert!(written.contains("nsec_privkey"));
    }

    #[test]
    fn wizard_save_refuses_a_preexisting_file() {
        let root = temp_root("existing");
        let config_file = root.join("settings.toml");
        std::fs::write(&config_file, "operator contents").expect("seed file");
        assert!(save_settings(&config_file, &sample_settings()).is_err());
        assert_eq!(
            std::fs::read_to_string(&config_file).expect("read back"),
            "operator contents"
        );
    }

    #[cfg(unix)]
    #[test]
    fn wizard_save_leaves_a_symlink_target_untouched() {
        let root = temp_root("symlink");
        let victim = root.join("victim");
        std::fs::write(&victim, "victim contents").expect("seed victim");
        set_mode(&victim, 0o644);

        let config_file = root.join("settings.toml");
        std::os::unix::fs::symlink(&victim, &config_file).expect("plant symlink");

        // The nsec must not be written through a link another local account
        // could have planted in the settings directory.
        assert!(save_settings(&config_file, &sample_settings()).is_err());
        assert_eq!(
            std::fs::read_to_string(&victim).expect("read back"),
            "victim contents"
        );
        assert_mode(&victim, 0o644);
    }

    #[test]
    fn manual_template_copy_is_owner_only() {
        let root = temp_root("template");
        let config_file = root.join("settings.toml");
        // The manual branch of `run_setup_menu` writes TEMPLATE_BYTES through
        // the same primitive; the menu itself needs a terminal, so exercise
        // the write it performs.
        create_owner_only(&config_file, TEMPLATE_BYTES).expect("write template");
        assert_mode(&config_file, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn manual_template_copy_refuses_a_symlink() {
        let root = temp_root("template-symlink");
        let victim = root.join("victim");
        std::fs::write(&victim, "victim contents").expect("seed victim");
        let linked = root.join("linked.toml");
        std::os::unix::fs::symlink(&victim, &linked).expect("plant symlink");
        assert!(create_owner_only(&linked, TEMPLATE_BYTES).is_err());
        assert_eq!(
            std::fs::read_to_string(&victim).expect("read back"),
            "victim contents"
        );
    }
}
