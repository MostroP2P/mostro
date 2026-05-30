use cdk::error::Error as CdkClientError;
use cdk::mint_url::MintUrl;
use cdk::wallet::MintConnector;
use cdk::nuts::{nut01::SecretKey as NutSecretKey, nut00::Proofs, nut10::SpendingConditions};
use cdk::nuts::{CheckStateRequest, CheckStateResponse, PublicKey, Token, nut02::ShortKeysetId};

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
        let url = MintUrl::from_str(mint_url)
            .map_err(|e| Error::InvalidMintUrl(e.to_string()))?;

        let client = cdk::HttpClient::new(url.clone(), None);
        let cashu_client = Self {
            mint_url: url.clone(),
            client,
        };

        match cashu_client.client.get_mint_info().await {
            Ok(info) => {
                if !info.nuts.nut11.supported {
                    CASHU_STATUS.get_or_init(|| false);
                    return Err(Error::MintConnection("Mint does not support NUT-11 P2PK".into()));
                }
                CASHU_STATUS.get_or_init(|| true);
                Ok(cashu_client)
            }
            Err(e) => {
                CASHU_STATUS.get_or_init(|| false);
                let err: cdk::error::Error = e.into();
                Err(Error::MintConnection(err.to_string()))
            }
        }
    }

    /// Verifies the 2-of-3 condition embedded in a token matches the expected pubkeys.
    /// It asserts that p_b, p_s, and p_m are present in the conditions.
    pub fn verify_2of3_condition(
        token: &str,
        p_b: PublicKey,
        p_s: PublicKey,
        p_m: PublicKey,
    ) -> Result<Token, Error> {
        let token = Token::from_str(token)
            .map_err(|e| Error::Token(e.to_string()))?;

        let secrets = token.token_secrets();
        if secrets.is_empty() {
            return Err(Error::Token("Token contains no secrets".into()));
        }

        for secret in secrets {
            let spending_conditions = SpendingConditions::try_from(secret).map_err(|e| Error::Condition(e.to_string()))?;
            
            if spending_conditions.num_sigs() != Some(2) {
                return Err(Error::Condition("Spending condition must require exactly 2 signatures".into()));
            }

            if spending_conditions.locktime().is_some() {
                return Err(Error::Condition("Spending condition cannot have a locktime".into()));
            }

            if spending_conditions.refund_keys().is_some() {
                return Err(Error::Condition("Spending condition cannot have refund keys".into()));
            }

            let pubkeys = spending_conditions.pubkeys().unwrap_or_default();
            if pubkeys.len() != 3 || !pubkeys.contains(&p_b) || !pubkeys.contains(&p_s) || !pubkeys.contains(&p_m) {
                return Err(Error::Condition("Missing expected pubkeys in spending condition".into()));
            }
        }

        Ok(token)
    }

    /// Checks the state of proofs against the mint's `/v1/checkstate` endpoint.
    /// Note: This only checks if the secrets are unspent. It does not authenticate
    /// that the proofs were signed by the mint. Use `verify_token_dleq` for that.
    pub async fn check_state(&self, ys: Vec<PublicKey>) -> Result<CheckStateResponse, Error> {
        let request = CheckStateRequest { ys };
        let response = self.client.post_check_state(request).await
            .map_err(|e| {
                Error::Client(cdk::error::Error::from(e))
            })?;
        Ok(response)
    }

    /// Verifies the DLEQ proofs for all proofs in a token.
    /// This authenticates that the token was actually issued by the mint.
    pub async fn verify_token_dleq(&self, token: &Token) -> Result<(), Error> {
        let keysets = self.client.get_mint_keys().await.map_err(|e| Error::Client(cdk::error::Error::from(e)))?;
        
        match token {
            Token::TokenV3(token_v3) => {
                let proofs = token_v3.token.iter().flat_map(|t| t.proofs.clone()).collect::<Vec<_>>();
                for proof in proofs {
                    let keyset = keysets.iter().find(|k| ShortKeysetId::from(k.id) == proof.keyset_id)
                        .ok_or_else(|| Error::Token("Unknown keyset".into()))?;
                    let mint_pubkey = keyset.keys.get(&proof.amount).ok_or_else(|| Error::Token("Unknown amount for keyset".into()))?;
                    
                    let p = proof.into_proof(&keyset.id);
                    p.verify_dleq(*mint_pubkey).map_err(|_| Error::Token("Invalid DLEQ proof".into()))?;
                }
            },
            Token::TokenV4(token_v4) => {
                for token_entry in &token_v4.token {
                    let keyset = keysets.iter().find(|k| ShortKeysetId::from(k.id) == token_entry.keyset_id)
                        .ok_or_else(|| Error::Token("Unknown keyset".into()))?;
                    
                    for proof_v4 in &token_entry.proofs {
                        let mint_pubkey = keyset.keys.get(&proof_v4.amount).ok_or_else(|| Error::Token("Unknown amount for keyset".into()))?;
                        let p = proof_v4.into_proof(&keyset.id);
                        p.verify_dleq(*mint_pubkey).map_err(|_| Error::Token("Invalid DLEQ proof".into()))?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Signs proofs using the arbitrator's (Mostro) secret key.
    pub fn sign_with_pm(proofs: &mut Proofs, p_m_secret: NutSecretKey) -> Result<(), Error> {
        for proof in proofs.iter_mut() {
            proof.sign_p2pk(p_m_secret.clone()).map_err(|e| Error::Client(cdk::error::Error::from(e)))?;
        }
        Ok(())
    }
}
