use crate::escrow::{EscrowBackend, HoldInvoice};
use async_trait::async_trait;
use cdk::error::Error as CdkClientError;
use cdk::mint_url::MintUrl;
use cdk::nuts::{nut00::Proofs, nut01::SecretKey as NutSecretKey, nut10::SpendingConditions};
use cdk::nuts::{nut02::ShortKeysetId, CheckStateRequest, CheckStateResponse, PublicKey, Token};
use cdk::wallet::MintConnector;
use mostro_core::prelude::*;

use std::str::FromStr;
use std::sync::OnceLock;
/// Error type for Cashu client operations
#[derive(Debug)]
pub enum Error {
    InvalidMintUrl(String),
    MintConnection(String),
    Token(String),
    Condition(String),
    Client(CdkClientError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::InvalidMintUrl(s) => write!(f, "Invalid mint URL: {}", s),
            Error::MintConnection(s) => write!(f, "Mint connection error: {}", s),
            Error::Token(s) => write!(f, "Token error: {}", s),
            Error::Condition(s) => write!(f, "Condition error: {}", s),
            Error::Client(e) => write!(f, "Client error: {}", e),
        }
    }
}

impl From<CdkClientError> for Error {
    fn from(e: CdkClientError) -> Self {
        Error::Client(e)
    }
}

/// A client for communicating with a Cashu mint.
#[derive(Clone)]
pub struct CashuClient {
    mint_url: MintUrl,
    client: cdk::HttpClient,
}

pub static CASHU_STATUS: OnceLock<bool> = OnceLock::new();

impl CashuClient {
    /// Connects to a mint URL and verifies it is reachable.
    pub async fn connect(mint_url: &str) -> Result<Self, Error> {
        let url = MintUrl::from_str(mint_url).map_err(|e| Error::InvalidMintUrl(e.to_string()))?;

        let client = cdk::HttpClient::new(url.clone(), None);
        let cashu_client = Self {
            mint_url: url.clone(),
            client,
        };

        match cashu_client.client.get_mint_info().await {
            Ok(info) => {
                if !info.nuts.nut11.supported {
                    CASHU_STATUS.get_or_init(|| false);
                    return Err(Error::MintConnection(
                        "Mint does not support NUT-11 P2PK".into(),
                    ));
                }
                CASHU_STATUS.get_or_init(|| true);
                Ok(cashu_client)
            }
            Err(e) => {
                CASHU_STATUS.get_or_init(|| false);
                Err(Error::MintConnection(e.to_string()))
            }
        }
    }

    /// The mint URL this client is bound to.
    ///
    /// Track A's lock handler reads this to persist the order's
    /// `cashu_mint_url` and to assert the seller's token was minted by the
    /// operator-configured mint.
    pub fn mint_url(&self) -> &MintUrl {
        &self.mint_url
    }

    /// Verifies the 2-of-3 condition embedded in a token matches the expected pubkeys.
    /// It asserts that p_b, p_s, and p_m are present in the conditions.
    pub fn verify_2of3_condition(
        token: &str,
        p_b: PublicKey,
        p_s: PublicKey,
        p_m: PublicKey,
    ) -> Result<Token, Error> {
        let token = Token::from_str(token).map_err(|e| Error::Token(e.to_string()))?;

        let secrets = token.token_secrets();
        if secrets.is_empty() {
            return Err(Error::Token("Token contains no secrets".into()));
        }

        for secret in secrets {
            let spending_conditions = SpendingConditions::try_from(secret)
                .map_err(|e| Error::Condition(e.to_string()))?;

            if spending_conditions.num_sigs() != Some(2) {
                return Err(Error::Condition(
                    "Spending condition must require exactly 2 signatures".into(),
                ));
            }

            if spending_conditions.locktime().is_some() {
                return Err(Error::Condition(
                    "Spending condition cannot have a locktime".into(),
                ));
            }

            if spending_conditions.refund_keys().is_some() {
                return Err(Error::Condition(
                    "Spending condition cannot have refund keys".into(),
                ));
            }

            let pubkeys = spending_conditions.pubkeys().unwrap_or_default();
            if pubkeys.len() != 3
                || !pubkeys.contains(&p_b)
                || !pubkeys.contains(&p_s)
                || !pubkeys.contains(&p_m)
            {
                return Err(Error::Condition(
                    "Missing expected pubkeys in spending condition".into(),
                ));
            }
        }

        Ok(token)
    }

    /// Full escrow-token validation for Track A's lock handler.
    ///
    /// Runs every check Mostro performs on a seller-submitted Cashu escrow
    /// token before accepting it as the locked trade funds, returning the parsed
    /// [`Token`] on success:
    ///
    /// 1. **2-of-3 condition.** [`Self::verify_2of3_condition`] asserts every
    ///    proof is P2PK-locked to a 2-of-3 over exactly `{p_b, p_s, p_m}` (the
    ///    order's buyer/seller trade pubkeys and Mostro's arbitrator key).
    /// 2. **Mint binding.** The token's mint URL must match the node's
    ///    configured mint (`self.mint_url`); Mostro only escrows on its own mint.
    /// 3. **Amount.** The token's total value must equal `expected_amount`
    ///    (sats); `check_state` only proves unspent-ness, not quantity.
    /// 4. **Unspent.** Every proof must be `Unspent` at the mint (NUT-07
    ///    `/v1/checkstate`). The checkstate `Y` points are derived directly from
    ///    each proof secret via `hash_to_curve`, so this needs no keyset fetch.
    ///
    /// Mint authenticity of the proofs (DLEQ) is intentionally out of scope here
    /// (see [`Self::verify_token_dleq`]); the parties already agreed on the mint.
    pub async fn verify_escrow_token(
        &self,
        token_str: &str,
        p_b: PublicKey,
        p_s: PublicKey,
        p_m: PublicKey,
        expected_amount: u64,
    ) -> Result<Token, Error> {
        // 1: parse and verify the 2-of-3 spending condition.
        let token = Self::verify_2of3_condition(token_str, p_b, p_s, p_m)?;

        // 2: the token must be hosted on the node's configured mint.
        let token_mint = token
            .mint_url()
            .map_err(|e| Error::Token(format!("token mint url: {e}")))?;
        if token_mint != self.mint_url {
            return Err(Error::Token(format!(
                "token mint {token_mint} does not match configured mint {}",
                self.mint_url
            )));
        }

        // 3: the locked amount must equal the order amount exactly.
        let value = token
            .value()
            .map_err(|e| Error::Token(format!("token value: {e}")))?
            .to_u64();
        if value != expected_amount {
            return Err(Error::Token(format!(
                "token amount {value} does not match expected {expected_amount}"
            )));
        }

        // 4: every proof must be unspent at the mint. Derive the checkstate Y
        // points from the proof secrets (Y = hash_to_curve(secret)).
        let secrets = token.token_secrets();
        if secrets.is_empty() {
            return Err(Error::Token("token contains no proofs".into()));
        }
        let ys = secrets
            .iter()
            .map(|s| cdk::dhke::hash_to_curve(s.as_bytes()))
            .collect::<Result<Vec<PublicKey>, _>>()
            .map_err(|e| Error::Token(format!("proof Y: {e}")))?;
        let states = self.check_state(ys).await?;
        if states
            .states
            .iter()
            .any(|s| s.state != cdk::nuts::State::Unspent)
        {
            return Err(Error::Token("one or more proofs are not unspent".into()));
        }

        Ok(token)
    }

    /// Checks the state of proofs against the mint's `/v1/checkstate` endpoint.
    /// Note: This only checks if the secrets are unspent. It does not authenticate
    /// that the proofs were signed by the mint. Use `verify_token_dleq` for that.
    pub async fn check_state(&self, ys: Vec<PublicKey>) -> Result<CheckStateResponse, Error> {
        let request = CheckStateRequest { ys };
        let response = self
            .client
            .post_check_state(request)
            .await
            .map_err(Error::Client)?;
        Ok(response)
    }

    /// Verifies the DLEQ proofs for all proofs in a token.
    /// This authenticates that the token was actually issued by the mint.
    pub async fn verify_token_dleq(&self, token: &Token) -> Result<(), Error> {
        let keysets = self.client.get_mint_keys().await.map_err(Error::Client)?;

        match token {
            Token::TokenV3(token_v3) => {
                let proofs = token_v3
                    .token
                    .iter()
                    .flat_map(|t| t.proofs.clone())
                    .collect::<Vec<_>>();
                for proof in proofs {
                    let keyset = keysets
                        .iter()
                        .find(|k| ShortKeysetId::from(k.id) == proof.keyset_id)
                        .ok_or_else(|| Error::Token("Unknown keyset".into()))?;
                    let mint_pubkey = keyset
                        .keys
                        .get(&proof.amount)
                        .ok_or_else(|| Error::Token("Unknown amount for keyset".into()))?;

                    let p = proof.into_proof(&keyset.id);
                    p.verify_dleq(*mint_pubkey)
                        .map_err(|_| Error::Token("Invalid DLEQ proof".into()))?;
                }
            }
            Token::TokenV4(token_v4) => {
                for token_entry in &token_v4.token {
                    let keyset = keysets
                        .iter()
                        .find(|k| ShortKeysetId::from(k.id) == token_entry.keyset_id)
                        .ok_or_else(|| Error::Token("Unknown keyset".into()))?;

                    for proof_v4 in &token_entry.proofs {
                        let mint_pubkey = keyset
                            .keys
                            .get(&proof_v4.amount)
                            .ok_or_else(|| Error::Token("Unknown amount for keyset".into()))?;
                        let p = proof_v4.into_proof(&keyset.id);
                        p.verify_dleq(*mint_pubkey)
                            .map_err(|_| Error::Token("Invalid DLEQ proof".into()))?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Signs proofs using the arbitrator's (Mostro) secret key.
    pub fn sign_with_pm(proofs: &mut Proofs, p_m_secret: NutSecretKey) -> Result<(), Error> {
        for proof in proofs.iter_mut() {
            proof
                .sign_p2pk(p_m_secret.clone())
                .map_err(|e| Error::Client(cdk::error::Error::from(e)))?;
        }
        Ok(())
    }
}

/// Convert a Nostr (BIP340 x-only) public key, given as 64-char hex, into a
/// Cashu (compressed secp256k1) [`PublicKey`].
///
/// Mostro's per-order trade keys and node identity key are x-only (32 bytes);
/// a NUT-11 P2PK spending condition needs the 33-byte compressed form. We
/// prepend the `0x02` (even-Y) parity byte, which is the same convention
/// `cdk::dhke::hash_to_curve` and the Cashu P2PK tooling use, so a signature
/// produced by the trade key validates against this derived pubkey.
///
/// Track A uses this to derive the expected `{P_B, P_S, P_M}` from the order's
/// trade pubkeys and Mostro's key, rather than trusting the pubkeys the seller
/// states in the submitted [`mostro_core`] lock proof.
pub fn cashu_pubkey_from_xonly_hex(xonly_hex: &str) -> Result<PublicKey, Error> {
    if xonly_hex.len() != 64 {
        return Err(Error::Condition(format!(
            "expected 64-char x-only hex, got {}",
            xonly_hex.len()
        )));
    }
    PublicKey::from_hex(format!("02{xonly_hex}"))
        .map_err(|e| Error::Condition(format!("pubkey convert: {e}")))
}

/// Cashu 2-of-3 multisig escrow backend.
///
/// Implements the [`EscrowBackend`] seam for Cashu mode. Mostro is only a
/// coordinator here — it never takes custody — so the lock validates a
/// seller-submitted token against the configured mint and the order's trade
/// pubkeys, while release / cancel / dispute settlement are P2P or
/// arbitrator-signed by the feature tracks.
///
/// The methods are filled in incrementally by the Cashu feature tracks
/// (Track A: [`EscrowBackend::lock`]; Tracks B/C/D: the rest). Until a track
/// lands, its method is `unimplemented!()`; the daemon only ever instantiates
/// this backend in Cashu mode, where `run_cashu` gates which actions dispatch.
#[derive(Debug, Default, Clone, Copy)]
pub struct CashuBackend;

impl CashuBackend {
    /// Create a new Cashu escrow backend.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl EscrowBackend for CashuBackend {
    async fn lock(
        &self,
        _order: &Order,
        _description: &str,
        _amount: i64,
    ) -> Result<HoldInvoice, MostroError> {
        unimplemented!("Cashu escrow lock is implemented in the Cashu lock track (Track A)")
    }

    async fn release(&self, _order: &Order) -> Result<(), MostroError> {
        unimplemented!("Cashu escrow release is implemented in the Cashu release track (Track B)")
    }

    async fn cooperative_cancel(&self, _order: &Order) -> Result<(), MostroError> {
        unimplemented!(
            "Cashu cooperative cancel is implemented in the Cashu cancel track (Track C)"
        )
    }

    async fn dispute_settle(&self, _order: &Order) -> Result<(), MostroError> {
        unimplemented!("Cashu dispute settle is implemented in the Cashu dispute track (Track D)")
    }

    async fn dispute_cancel(&self, _order: &Order) -> Result<(), MostroError> {
        unimplemented!("Cashu dispute cancel is implemented in the Cashu dispute track (Track D)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid 32-byte x-only hex (a known secp256k1 x coordinate).
    const XONLY_HEX: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn xonly_to_cashu_pubkey_prepends_even_parity() {
        let pk = cashu_pubkey_from_xonly_hex(XONLY_HEX).expect("valid x-only converts");
        // The compressed form is the even-parity (0x02) point over the x-only.
        assert_eq!(pk.to_hex(), format!("02{XONLY_HEX}"));
    }

    #[test]
    fn xonly_to_cashu_pubkey_rejects_wrong_length() {
        // 63 chars (too short) and the already-prefixed 66-char form must both
        // be rejected: the helper expects exactly the 64-char x-only.
        assert!(cashu_pubkey_from_xonly_hex(&XONLY_HEX[..63]).is_err());
        assert!(cashu_pubkey_from_xonly_hex(&format!("02{XONLY_HEX}")).is_err());
    }

    #[test]
    fn xonly_to_cashu_pubkey_rejects_non_hex() {
        // Right length, but not valid hex / not a curve point.
        let not_hex = "z".repeat(64);
        assert!(cashu_pubkey_from_xonly_hex(&not_hex).is_err());
    }
}
