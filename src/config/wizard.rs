use std::io::Write;
use std::path::{Path, PathBuf};

use dialoguer::{Confirm, Input, Select};
use mostro_core::error::MostroError::{self, MostroInternalErr};
use mostro_core::error::ServiceError;
use nostr_sdk::prelude::*;

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
            std::fs::write(config_file_path, TEMPLATE_BYTES)
                .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;
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

    let nostr = prompt_nostr_settings()?;

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
    };

    let toml_content = toml::to_string_pretty(&settings)
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

    {
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(config_file_path)
        };
        #[cfg(not(unix))]
        let file = {
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(config_file_path)
        };
        let mut file = file.map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;
        file.write_all(toml_content.as_bytes())
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;
    }

    println!("\nConfiguration saved to {}\n", config_file_path.display());

    // Override database URL to use settings directory
    let mut settings = settings;
    settings.database.url = format!("sqlite://{}", settings_dir.join("mostro.db").display());

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
        lndk_enabled: false,
        lndk_grpc_host: "https://127.0.0.1:7000".to_string(),
        lndk_cert_file: String::new(),
        lndk_macaroon_file: String::new(),
        lndk_fetch_invoice_timeout: 60,
        lndk_fee_limit_percent: None,
        bip353_enabled: false,
        bip353_doh_resolver: "https://1.1.1.1/dns-query".to_string(),
        bip353_skip_dnssec: false,
    })
}

fn prompt_nostr_settings() -> Result<NostrSettings, MostroError> {
    let has_nsec = Confirm::new()
        .with_prompt("Do you have an existing nsec key?")
        .default(false)
        .interact()
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

    let nsec_privkey = if has_nsec {
        Input::new()
            .with_prompt("Enter your nsec private key")
            .validate_with(|input: &String| validate_nsec(input))
            .interact_text()
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?
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
        println!("  nsec: {}", nsec);
        println!("  npub: {}", npub);
        println!("\n  IMPORTANT: Save your nsec in a secure place. You will need it to recover your Mostro identity.\n");

        nsec
    };

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
}
