use cdk::error::Error as CdkClientError;
use cdk::mint_url::MintUrl;
use cdk::wallet::MintConnector;
use cdk::nuts::{nut01::SecretKey as NutSecretKey, nut00::Proofs, nut10::SpendingConditions};
use cdk::nuts::{CheckStateRequest, CheckStateResponse, PublicKey, Token};

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

        match cashu_client.client.get_mint_keys().await {
            Ok(_) => {
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

        match &token {
            Token::TokenV3(token_v3) => {
                for token_entry in &token_v3.token {
                    for proof in &token_entry.proofs {
                        let secret = proof.secret.clone();
                        
                        let spending_conditions = SpendingConditions::try_from(&secret).map_err(|e| Error::Condition(e.to_string()))?;
                        if let SpendingConditions::P2PKConditions { data: _data, conditions } = spending_conditions {
                            if let Some(conds) = conditions {
                                if let Some(pubkeys) = conds.pubkeys {
                                    if !pubkeys.contains(&p_b) || !pubkeys.contains(&p_s) || !pubkeys.contains(&p_m) {
                                        return Err(Error::Condition("Missing expected pubkeys in spending condition".into()));
                                    }
                                } else {
                                    return Err(Error::Condition("No pubkeys found in spending conditions".into()));
                                }
                            } else {
                                return Err(Error::Condition("No conditions found in P2PK".into()));
                            }
                        } else {
                            return Err(Error::Condition("Not a P2PK spending condition".into()));
                        }
                    }
                }
            }
            Token::TokenV4(_) => {
                return Err(Error::Token("TokenV4 not supported for spending condition verification without keysets".into()));
            }
        }

        Ok(token)
    }

    /// Checks the state of proofs against the mint's `/v1/checkstate` endpoint.
    /// Returns a list of booleans indicating if each proof is unspent.
    pub async fn check_state(&self, ys: Vec<PublicKey>) -> Result<CheckStateResponse, Error> {
        let request = CheckStateRequest { ys };
        let response = self.client.post_check_state(request).await
            .map_err(|e| {
                Error::Client(cdk::error::Error::from(e))
            })?;
        Ok(response)
    }

    /// Signs proofs using the arbitrator's (Mostro) secret key.
    pub fn sign_with_pm(proofs: &mut Proofs, p_m_secret: NutSecretKey) -> Result<(), Error> {
        for proof in proofs.iter_mut() {
            proof.sign_p2pk(p_m_secret.clone()).map_err(|e| Error::Client(cdk::error::Error::from(e)))?;
        }
        Ok(())
    }
}
