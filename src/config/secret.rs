//! Helpers for loading and parsing the Mostro Nostr private key with
//! zeroization of transient buffers.

use crate::config::constants::{NSEC_ENV_VAR, RPC_TOKEN_ENV_VAR};
use crate::config::types::NostrSettings;
use mostro_core::error::MostroError::{self, *};
use mostro_core::error::ServiceError;
use nostr_sdk::prelude::Keys;
use secrecy::{ExposeSecret, SecretString};
use serde::Serializer;
use zeroize::Zeroize;

/// Serialize a [`SecretString`] for config files (wizard / TOML export only).
pub fn serialize_nsec<S>(secret: &SecretString, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(secret.expose_secret())
}

/// Read `name` from the process environment, trim whitespace, and wrap in a
/// [`SecretString`], zeroizing the transient buffer. Returns `None` when the
/// variable is unset or blank.
fn read_secret_env_var(name: &str) -> Option<SecretString> {
    let mut value_from_env = std::env::var(name).ok()?;
    let trimmed = value_from_env.trim();
    if trimmed.is_empty() {
        value_from_env.zeroize();
        return None;
    }
    let secret = SecretString::from(trimmed.to_owned());
    value_from_env.zeroize();
    Some(secret)
}

/// Read `MOSTRO_NSEC_PRIVKEY` from the process environment, trim whitespace,
/// and wrap in a [`SecretString`]. Returns `None` when unset or blank.
pub fn read_nsec_env_var() -> Option<SecretString> {
    read_secret_env_var(NSEC_ENV_VAR)
}

/// Read `MOSTRO_RPC_TOKEN` from the process environment, trim whitespace, and
/// wrap in a [`SecretString`]. Returns `None` when unset or blank.
///
/// The admin RPC bearer token is env-only by design: `settings.toml` is the
/// file operators paste into issues when asking for help.
pub fn read_rpc_token_env_var() -> Option<SecretString> {
    read_secret_env_var(RPC_TOKEN_ENV_VAR)
}

/// Parse a bech32 nsec into [`Keys`], exposing the secret only in this scope.
pub fn parse_mostro_keys(secret: &SecretString) -> Result<Keys, MostroError> {
    let nsec = secret.expose_secret();
    if nsec.is_empty() {
        return Err(MostroInternalErr(ServiceError::NostrError(
            "Nostr private key is not configured".to_string(),
        )));
    }
    Keys::parse(nsec).map_err(|e| {
        tracing::error!("Failed to parse nostr private key: {}", e);
        MostroInternalErr(ServiceError::NostrError(e.to_string()))
    })
}

/// Take the nsec from nostr settings (env override must already be applied),
/// parse it into [`Keys`], and clear the field so global settings no longer
/// retain plaintext.
pub fn take_nsec_for_init(nostr: &mut NostrSettings) -> Result<Keys, MostroError> {
    let secret = std::mem::take(&mut nostr.nsec_privkey);
    let keys = parse_mostro_keys(&secret)?;
    nostr.nsec_privkey = SecretString::default();
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[test]
    fn take_nsec_clears_settings_field() {
        let mut nostr = NostrSettings {
            nsec_privkey: SecretString::from(
                "nsec13as48eum93hkg7plv526r9gjpa0uc52zysqm93pmnkca9e69x6tsdjmdxd",
            ),
            relays: vec![],
        };
        let keys = take_nsec_for_init(&mut nostr).expect("valid test nsec");
        assert!(nostr.nsec_privkey.expose_secret().is_empty());
        assert!(!keys.public_key().to_hex().is_empty());
    }

    #[test]
    fn parse_mostro_keys_rejects_empty() {
        let err = parse_mostro_keys(&SecretString::default()).unwrap_err();
        assert!(matches!(
            err,
            MostroInternalErr(ServiceError::NostrError(_))
        ));
    }
}
