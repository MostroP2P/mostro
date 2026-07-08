//! Cashu mint client — Cashu foundation **CF-2**
//! (see `docs/cashu/01-fundamentals.md` §6).
//!
//! A self-contained `cdk` wrapper: parse/validate escrow tokens and talk to
//! a mint. Pure library — **not wired into the daemon** (CF-5 does the boot
//! wiring; Track A adds the first caller). Adapted from the reviewed
//! first-attempt module (PR #765), updated to `cdk 0.17.2` and to the
//! re-planned spec: the 2-of-3 escrow condition now **requires** the
//! seller-recovery locktime pathway (`locktime` + `refund = [P_S]`,
//! Track A §4B) and every amount check is pinned to `sat` keysets (M-3).

use std::collections::HashSet;
use std::str::FromStr;
use std::time::Duration;

use cdk::error::Error as CdkClientError;
use cdk::mint_url::MintUrl;
use cdk::nuts::nut02::ShortKeysetId;
use cdk::nuts::nut10::{Secret as Nut10Secret, SpendingConditions, TagKind};
use cdk::nuts::{
    CheckStateRequest, CheckStateResponse, CurrencyUnit, PublicKey, SigFlag, State, Token,
};
use cdk::wallet::MintConnector;
use cdk::HttpClient;

/// Per-request bound on every mint HTTP call. `HttpClient` wraps a
/// `reqwest` client with **no default timeout**, so without this a slow or
/// unreachable mint could hang `connect`, `check_state` or the DLEQ keyset
/// fetch indefinitely, stranding the calling handler.
const MINT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Error type for Cashu client operations.
#[derive(Debug)]
pub enum Error {
    /// The configured mint URL could not be parsed.
    InvalidMintUrl(String),
    /// The mint is unreachable or missing a required NUT / `sat` keyset.
    MintConnection(String),
    /// The token is malformed, on the wrong mint/unit, mis-valued, not
    /// mint-issued, or not unspent.
    Token(String),
    /// The NUT-10/11 spending condition does not match the escrow shape.
    Condition(String),
    /// An underlying `cdk` client error.
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

impl std::error::Error for Error {}

impl From<CdkClientError> for Error {
    fn from(e: CdkClientError) -> Self {
        Error::Client(e)
    }
}

/// A client for communicating with a Cashu mint.
#[derive(Clone)]
pub struct CashuClient {
    mint_url: MintUrl,
    client: HttpClient,
}

impl CashuClient {
    /// Connect to a mint URL and verify it can back the escrow flows.
    ///
    /// Refuses to connect (rather than failing every later
    /// `AddCashuEscrow`, stranding orders in `WaitingPayment`) when the
    /// mint is unreachable, is missing a required NUT — 11 (P2PK 2-of-3),
    /// 07 (`/v1/checkstate`), 12 (DLEQ) — or exposes **no active `sat`
    /// keyset** (M-3: every amount comparison downstream is in sats, so a
    /// mint that cannot issue sat ecash is unusable; mints that *also*
    /// serve other units are fine — the per-proof keyset-unit check in
    /// [`Self::verify_token_dleq`] keeps foreign-unit tokens out).
    pub async fn connect(mint_url: &str) -> Result<Self, Error> {
        let url = MintUrl::from_str(mint_url).map_err(|e| Error::InvalidMintUrl(e.to_string()))?;

        let client = HttpClient::new(url.clone(), None);
        let cashu_client = Self {
            mint_url: url,
            client,
        };

        let info = tokio::time::timeout(MINT_REQUEST_TIMEOUT, cashu_client.client.get_mint_info())
            .await
            .map_err(|_| Error::MintConnection("timed out fetching mint info".into()))?
            .map_err(|e| Error::MintConnection(e.to_string()))?;
        if !info.nuts.nut11.supported {
            return Err(Error::MintConnection(
                "Mint does not support NUT-11 P2PK".into(),
            ));
        }
        if !info.nuts.nut07.supported {
            return Err(Error::MintConnection(
                "Mint does not support NUT-07 token state check".into(),
            ));
        }
        if !info.nuts.nut12.supported {
            return Err(Error::MintConnection(
                "Mint does not support NUT-12 DLEQ proofs".into(),
            ));
        }

        let keysets =
            tokio::time::timeout(MINT_REQUEST_TIMEOUT, cashu_client.client.get_mint_keysets())
                .await
                .map_err(|_| Error::MintConnection("timed out fetching mint keysets".into()))?
                .map_err(|e| Error::MintConnection(e.to_string()))?;
        if !keysets
            .keysets
            .iter()
            .any(|ks| ks.active && ks.unit == CurrencyUnit::Sat)
        {
            return Err(Error::MintConnection(
                "Mint has no active sat keyset".into(),
            ));
        }

        Ok(cashu_client)
    }

    /// The mint URL this client is bound to.
    ///
    /// Track A's lock handler reads this to persist the order's
    /// `cashu_mint_url` and to assert the seller's token was minted by the
    /// operator-configured mint.
    pub fn mint_url(&self) -> &MintUrl {
        &self.mint_url
    }

    /// Verify the escrow spending condition on every proof of a token
    /// (Track A §4/§4B). Each proof must be P2PK-locked with:
    ///
    /// - exactly 2 required signatures (`n_sigs = 2`) over exactly the
    ///   three expected pubkeys `{p_b, p_s, p_m}` (no missing, wrong,
    ///   extra, or duplicated key);
    /// - a **`locktime` tag present** — NUT-11 makes a locktime'd token
    ///   with no `refund` tag spendable by *anyone* after expiry, so —
    /// - **`refund = [p_s]`** exactly (the seller-recovery pathway) with
    ///   `n_sigs_refund = 1` (absent means 1 per NUT-11).
    ///
    /// The locktime *value* (floor check) is verified by
    /// [`verify_escrow_conditions`] / [`Self::verify_escrow_token`], which
    /// receive the config-derived minimum.
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

        let expected: HashSet<PublicKey> = [p_b, p_s, p_m].into_iter().collect();
        if expected.len() != 3 {
            return Err(Error::Condition(
                "Buyer, seller and Mostro pubkeys must be distinct".into(),
            ));
        }

        for secret in secrets {
            // NUT-11 marks a secret carrying duplicate standard tags as
            // malformed, but cdk's `Conditions` parser silently keeps only
            // the first occurrence of each and drops the rest. Inspect the
            // raw NUT-10 tag list and reject duplicates *before* that lossy
            // conversion; otherwise a forged second `refund`/`locktime`/
            // `n_sigs`/`sigflag`/… tag passes every check below while a
            // non-cdk mint could settle the token under different spend
            // conditions than the daemon verified.
            let nut10_secret =
                Nut10Secret::try_from(secret).map_err(|e| Error::Condition(e.to_string()))?;
            reject_duplicate_standard_tags(&nut10_secret)?;

            let spending_conditions = SpendingConditions::try_from(nut10_secret)
                .map_err(|e| Error::Condition(e.to_string()))?;

            // Only NUT-11 P2PK backs the escrow; an HTLC condition with
            // matching tags must not slip through.
            let (data, conditions) = match spending_conditions {
                SpendingConditions::P2PKConditions { data, conditions } => (data, conditions),
                other => {
                    return Err(Error::Condition(format!(
                        "Spending condition kind {:?} is not P2PK",
                        other.kind()
                    )));
                }
            };
            let conditions = conditions
                .ok_or_else(|| Error::Condition("P2PK condition carries no NUT-11 tags".into()))?;

            // The documented release/refund flow signs inputs only
            // (`SIG_INPUTS`): the seller signs the escrow once and the buyer
            // later chooses their own swap outputs at redeem time. A
            // `SIG_ALL` token instead binds specific outputs and cannot be
            // redeemed by that flow, stranding the locked order — reject any
            // escrow whose sig flag is not `SIG_INPUTS`.
            if conditions.sig_flag != SigFlag::SigInputs {
                return Err(Error::Condition(format!(
                    "escrow sig flag {:?} is not SIG_INPUTS",
                    conditions.sig_flag
                )));
            }

            if conditions.num_sigs != Some(2) {
                return Err(Error::Condition(
                    "Spending condition must require exactly 2 signatures".into(),
                ));
            }

            // The signing set is `data` plus the `pubkeys` tag; it must be
            // exactly the three expected keys, with no duplicates faking a
            // larger set.
            let mut signing_set: HashSet<PublicKey> = HashSet::from([data]);
            let extra_pubkeys = conditions.pubkeys.clone().unwrap_or_default();
            let stated_len = 1 + extra_pubkeys.len();
            signing_set.extend(extra_pubkeys);
            if signing_set.len() != stated_len {
                return Err(Error::Condition(
                    "Spending condition repeats a pubkey".into(),
                ));
            }
            if signing_set != expected {
                return Err(Error::Condition(
                    "Spending condition pubkeys do not match the expected {buyer, seller, mostro}"
                        .into(),
                ));
            }

            // Seller-recovery pathway (Track A §4B). A locktime with no
            // refund key would make the token anyone-can-spend after
            // expiry; a missing locktime would lock the funds forever if
            // Mostro disappears and the buyer won't cooperate.
            if conditions.locktime.is_none() {
                return Err(Error::Condition(
                    "Spending condition must carry a locktime (seller-recovery pathway)".into(),
                ));
            }
            match conditions.refund_keys.as_deref() {
                Some([refund]) if *refund == p_s => {}
                _ => {
                    return Err(Error::Condition(
                        "Refund keys must be exactly [seller]".into(),
                    ));
                }
            }
            // NUT-11: absent n_sigs_refund defaults to 1.
            if let Some(n) = conditions.num_sigs_refund {
                if n != 1 {
                    return Err(Error::Condition(format!(
                        "n_sigs_refund must be 1, got {n}"
                    )));
                }
            }
        }

        Ok(token)
    }

    /// Full escrow-token validation for Track A's lock handler.
    ///
    /// Composes every check Mostro performs on a seller-submitted escrow
    /// token before accepting it as the locked trade funds, returning the
    /// parsed [`Token`] on success:
    ///
    /// 1. **Condition + locktime floor** ([`verify_escrow_conditions`]):
    ///    2-of-3 over `{p_b, p_s, p_m}` with the seller-recovery pathway,
    ///    and every `locktime >= min_locktime` (the caller passes
    ///    `now + cashu.escrow_locktime_days`; a seller may set a *longer*
    ///    locktime, never a shorter one — Track A §4B).
    /// 2. **Mint binding.** The token's mint URL must match the node's
    ///    configured mint; Mostro only escrows on its own mint.
    /// 3. **Amount.** Token-level unit must be `sat` and the total value
    ///    must equal `expected_amount` (sats).
    /// 4. **Mint-issued (DLEQ) + per-proof `sat` keyset**
    ///    ([`Self::verify_token_dleq`]). Without DLEQ a seller could
    ///    fabricate unspent-but-worthless proofs, since `check_state`
    ///    reports any unknown secret as `Unspent`.
    /// 5. **Unspent** (NUT-07 `/v1/checkstate`), failing closed if the
    ///    mint returns fewer states than proofs queried.
    pub async fn verify_escrow_token(
        &self,
        token_str: &str,
        p_b: PublicKey,
        p_s: PublicKey,
        p_m: PublicKey,
        expected_amount: u64,
        min_locktime: u64,
    ) -> Result<Token, Error> {
        // 1: spending condition + locktime floor (offline checks).
        let token = verify_escrow_conditions(token_str, p_b, p_s, p_m, min_locktime)?;

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

        // Reject a token that repeats a proof before its value is trusted: a
        // duplicated proof inflates `value()` below and is reported unspent
        // once per copy by checkstate (step 5), so one proof could satisfy a
        // larger escrow than it is actually worth.
        reject_duplicate_proofs(&token)?;

        // 3: the locked amount must equal the order amount exactly, and be
        // denominated in sats. `value()` is a bare integer, so without the
        // unit guard a mint exposing multiple units would let a
        // 100_000-msat (or usd-cent) token satisfy a 100_000-sat order.
        match token.unit() {
            Some(CurrencyUnit::Sat) => {}
            other => {
                return Err(Error::Token(format!("token unit {other:?} is not sat")));
            }
        }
        let value = token
            .value()
            .map_err(|e| Error::Token(format!("token value: {e}")))?
            .to_u64();
        if value != expected_amount {
            return Err(Error::Token(format!(
                "token amount {value} does not match expected {expected_amount}"
            )));
        }

        // 4: the proofs must be genuine, mint-issued ecash on sat keysets.
        // `check_state` (step 5) only proves a secret is unspent — an
        // honest mint reports any *unknown* secret as `Unspent`, so a
        // seller could fabricate proofs with the right 2-of-3 condition and
        // amount but no mint signature and still pass every other check,
        // tricking the buyer into sending fiat against worthless ecash.
        // DLEQ (NUT-12) authenticates each proof's blind signature against
        // the mint's keyset offline, closing that hole.
        self.verify_token_dleq(&token).await?;

        // 5: every proof must be unspent at the mint. Derive the checkstate
        // Y points from the proof secrets (Y = hash_to_curve(secret)).
        let secrets = token.token_secrets();
        let ys = secrets
            .iter()
            .map(|s| cdk::dhke::hash_to_curve(s.as_bytes()))
            .collect::<Result<Vec<PublicKey>, _>>()
            .map_err(|e| Error::Token(format!("proof Y: {e}")))?;
        let expected_states = ys.len();
        let states = self.check_state(ys).await?;
        // Fail closed if the mint returns fewer states than proofs queried:
        // a missing entry must never be treated as implicitly `Unspent`.
        if states.states.len() != expected_states {
            return Err(Error::Token(format!(
                "checkstate returned {} states for {expected_states} proofs",
                states.states.len()
            )));
        }
        if states.states.iter().any(|s| s.state != State::Unspent) {
            return Err(Error::Token("one or more proofs are not unspent".into()));
        }

        Ok(token)
    }

    /// Check the state of proofs against the mint's `/v1/checkstate`
    /// endpoint (NUT-07). This only proves the secrets are unspent — it
    /// does **not** authenticate that the proofs were signed by the mint;
    /// use [`Self::verify_token_dleq`] for that.
    pub async fn check_state(&self, ys: Vec<PublicKey>) -> Result<CheckStateResponse, Error> {
        let request = CheckStateRequest { ys };
        let response =
            tokio::time::timeout(MINT_REQUEST_TIMEOUT, self.client.post_check_state(request))
                .await
                .map_err(|_| Error::MintConnection("timed out checking proof state".into()))?
                .map_err(Error::Client)?;
        Ok(response)
    }

    /// Verify the NUT-12 DLEQ proof of every proof in a token — this
    /// authenticates the ecash as genuinely mint-issued — and that every
    /// proof belongs to a **`sat` keyset** (M-3: a token may mix keysets,
    /// so the connect-time check alone does not cover it).
    pub async fn verify_token_dleq(&self, token: &Token) -> Result<(), Error> {
        // TODO(track-b): `get_mint_keys` returns only ACTIVE keysets
        // (NUT-01), so a proof minted under a since-rotated keyset is
        // rejected here as "Unknown keyset" even though the mint still
        // honours it. Track A only ever sees freshly-minted tokens so this
        // is safe, but before Track B verifies older tokens this must fall
        // back to fetching the specific keyset via `/v1/keys/{keyset_id}`
        // for inactive keyset ids.
        let keysets = tokio::time::timeout(MINT_REQUEST_TIMEOUT, self.client.get_mint_keys())
            .await
            .map_err(|_| Error::MintConnection("timed out fetching mint keys".into()))?
            .map_err(Error::Client)?;

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
                    if keyset.unit != CurrencyUnit::Sat {
                        return Err(Error::Token(format!(
                            "proof keyset unit {:?} is not sat",
                            keyset.unit
                        )));
                    }
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
                    if keyset.unit != CurrencyUnit::Sat {
                        return Err(Error::Token(format!(
                            "proof keyset unit {:?} is not sat",
                            keyset.unit
                        )));
                    }

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
}

/// Reject a NUT-10 secret that repeats any standard NUT-11 tag.
///
/// NUT-11 marks a secret carrying duplicate standard tags as malformed, but
/// cdk's [`SpendingConditions`] parser keeps only the *first* occurrence of
/// each and silently drops the rest. Without this guard a forged secret
/// with a second `refund`, `locktime`, `n_sigs`, `n_sigs_refund`, `pubkeys`
/// or `sigflag` tag satisfies every check in
/// [`CashuClient::verify_2of3_condition`], yet a non-cdk mint could accept
/// or settle it under spend conditions the daemon never verified. Custom
/// (non-standard) tags may legitimately repeat and are ignored.
fn reject_duplicate_standard_tags(secret: &Nut10Secret) -> Result<(), Error> {
    let Some(tags) = secret.secret_data().tags() else {
        return Ok(());
    };
    let mut seen: HashSet<TagKind> = HashSet::new();
    for tag in tags {
        let Some(key) = tag.first() else {
            continue;
        };
        let kind = TagKind::from(key);
        if matches!(kind, TagKind::Custom(_)) {
            continue;
        }
        if !seen.insert(kind) {
            return Err(Error::Condition(format!(
                "duplicate NUT-11 `{key}` tag (malformed per NUT-11)"
            )));
        }
    }
    Ok(())
}

/// Reject a token that repeats a proof, keyed by its secret (which fixes
/// the checkstate `Y = hash_to_curve(secret)`).
///
/// [`Token::value`] sums *every* proof and NUT-07 `/v1/checkstate` answers
/// in request order, so a duplicated proof is both counted toward the
/// escrow amount **and** reported `Unspent` once per copy: a token backed by
/// a single 50-sat proof repeated twice would satisfy a 100-sat escrow even
/// though the mint will only ever let that proof be spent once. Deduplicate
/// the proof secrets before [`CashuClient::verify_escrow_token`]'s amount
/// and unspent checks trust the token value.
fn reject_duplicate_proofs(token: &Token) -> Result<(), Error> {
    let secrets = token.token_secrets();
    let mut seen: HashSet<&cdk::secret::Secret> = HashSet::with_capacity(secrets.len());
    for secret in secrets {
        if !seen.insert(secret) {
            return Err(Error::Token(
                "token repeats a proof (duplicate secret)".into(),
            ));
        }
    }
    Ok(())
}

/// Offline half of the escrow-token acceptance check: the 2-of-3 condition
/// ([`CashuClient::verify_2of3_condition`]) plus the **locktime floor** —
/// every proof's `locktime` must be `>= min_locktime`. Split out of
/// [`CashuClient::verify_escrow_token`] so it is unit-testable without a
/// mint; the async method composes this with the mint-backed checks.
///
/// The floor is the security edge of Track A §4B: once a locktime passes,
/// the seller can reclaim the sats *even if the buyer already sent fiat*,
/// so a seller-chosen short locktime must never be accepted.
pub fn verify_escrow_conditions(
    token_str: &str,
    p_b: PublicKey,
    p_s: PublicKey,
    p_m: PublicKey,
    min_locktime: u64,
) -> Result<Token, Error> {
    let token = CashuClient::verify_2of3_condition(token_str, p_b, p_s, p_m)?;

    for secret in token.token_secrets() {
        let conditions = match SpendingConditions::try_from(secret) {
            Ok(SpendingConditions::P2PKConditions {
                conditions: Some(c),
                ..
            }) => c,
            // verify_2of3_condition already guaranteed P2PK-with-tags.
            _ => unreachable!("verify_2of3_condition accepted a non-P2PK secret"),
        };
        // Presence already enforced by verify_2of3_condition; here we
        // enforce the floor.
        let locktime = conditions
            .locktime
            .expect("verify_2of3_condition requires a locktime");
        if locktime < min_locktime {
            return Err(Error::Condition(format!(
                "locktime {locktime} is below the required floor {min_locktime}"
            )));
        }
    }

    Ok(token)
}

/// Convert a Nostr (BIP340 x-only) public key, given as 64-char hex, into a
/// Cashu (compressed secp256k1) [`PublicKey`].
///
/// Mostro's per-order trade keys and node identity key are x-only (32
/// bytes); a NUT-11 P2PK spending condition needs the 33-byte compressed
/// form. We prepend the `0x02` (even-Y) parity byte, which is the same
/// convention the Cashu P2PK tooling uses; NUT-11 signatures are BIP340
/// Schnorr over the x-only coordinate, so a signature produced by the trade
/// key validates against this derived pubkey regardless of the source
/// point's parity (proven by the
/// `odd_y_nostr_key_signs_for_derived_cashu_pubkey` test).
///
/// Track A uses this to derive the expected `{P_B, P_S, P_M}` from the
/// order's trade pubkeys and Mostro's key, rather than trusting the pubkeys
/// the seller states in the submitted lock proof.
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

#[cfg(test)]
mod tests {
    use super::*;
    use cdk::nuts::nut00::{Proof, Proofs};
    use cdk::nuts::nut01::SecretKey as NutSecretKey;
    use cdk::nuts::nut02::Id;
    use cdk::nuts::nut10::Conditions;
    use cdk::nuts::SigFlag;
    use cdk::secret::Secret;
    use cdk::Amount;

    /// A valid 32-byte x-only hex (a known secp256k1 x coordinate).
    const XONLY_HEX: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const TEST_KEYSET_ID: &str = "009a1f293253e41e";
    const TEST_C: &str = "02698c4e2b5f9534cd0687d87513c759790cf829aa5739184a3e3735471fbda904";
    const TEST_MINT: &str = "https://mint.example.com";
    const LOCKTIME: u64 = 2_000_000_000;

    fn keypair(n: u8) -> PublicKey {
        let sk = NutSecretKey::from_hex(format!("{:064x}", n as u64 + 1)).unwrap();
        sk.public_key()
    }

    /// Build a serialized token whose single proof carries the given P2PK
    /// condition shape. `data` is the NUT-10 data pubkey; the rest land in
    /// the NUT-11 tags.
    fn token_with_condition(
        data: PublicKey,
        pubkeys: Vec<PublicKey>,
        num_sigs: Option<u64>,
        locktime: Option<u64>,
        refund_keys: Option<Vec<PublicKey>>,
        num_sigs_refund: Option<u64>,
    ) -> String {
        let secret: Secret = SpendingConditions::P2PKConditions {
            data,
            conditions: Some(Conditions {
                locktime,
                pubkeys: Some(pubkeys),
                refund_keys,
                num_sigs,
                sig_flag: SigFlag::SigInputs,
                num_sigs_refund,
            }),
        }
        .try_into()
        .expect("valid p2pk secret");

        let proof = Proof {
            keyset_id: Id::from_str(TEST_KEYSET_ID).unwrap(),
            amount: Amount::from(100u64),
            secret,
            c: PublicKey::from_str(TEST_C).unwrap(),
            witness: None,
            dleq: None,
            p2pk_e: None,
        };
        let proofs: Proofs = vec![proof];
        Token::new(
            MintUrl::from_str(TEST_MINT).unwrap(),
            proofs,
            None,
            CurrencyUnit::Sat,
        )
        .to_string()
    }

    /// Build a serialized single-proof token from a raw NUT-10 secret JSON
    /// string. Used to forge tag shapes cdk's own `Conditions`/constructor
    /// would reject or silently normalize (duplicate tags, `SIG_ALL`,
    /// `n_sigs_refund` extremes), which an attacker hands us directly.
    fn raw_secret_token(raw_secret: &str) -> String {
        let proof = Proof {
            keyset_id: Id::from_str(TEST_KEYSET_ID).unwrap(),
            amount: Amount::from(100u64),
            secret: Secret::new(raw_secret.to_string()),
            c: PublicKey::from_str(TEST_C).unwrap(),
            witness: None,
            dleq: None,
            p2pk_e: None,
        };
        Token::new(
            MintUrl::from_str(TEST_MINT).unwrap(),
            vec![proof],
            None,
            CurrencyUnit::Sat,
        )
        .to_string()
    }

    /// The §4B escrow shape: data = P_S, pubkeys = [P_B, P_M], n_sigs = 2,
    /// locktime present, refund = [P_S].
    fn valid_escrow_token(p_b: PublicKey, p_s: PublicKey, p_m: PublicKey) -> String {
        token_with_condition(
            p_s,
            vec![p_b, p_m],
            Some(2),
            Some(LOCKTIME),
            Some(vec![p_s]),
            None,
        )
    }

    #[test]
    fn accepts_valid_2of3_with_locktime_and_seller_refund() {
        let (p_b, p_s, p_m) = (keypair(1), keypair(2), keypair(3));
        let token = valid_escrow_token(p_b, p_s, p_m);
        assert!(CashuClient::verify_2of3_condition(&token, p_b, p_s, p_m).is_ok());

        // Explicit n_sigs_refund = 1 is equally valid (absent defaults to 1).
        let token = token_with_condition(
            p_s,
            vec![p_b, p_m],
            Some(2),
            Some(LOCKTIME),
            Some(vec![p_s]),
            Some(1),
        );
        assert!(CashuClient::verify_2of3_condition(&token, p_b, p_s, p_m).is_ok());

        // Key arrangement is set-based: data = P_B works too.
        let token = token_with_condition(
            p_b,
            vec![p_s, p_m],
            Some(2),
            Some(LOCKTIME),
            Some(vec![p_s]),
            None,
        );
        assert!(CashuClient::verify_2of3_condition(&token, p_b, p_s, p_m).is_ok());
    }

    #[test]
    fn rejects_wrong_sig_count() {
        let (p_b, p_s, p_m) = (keypair(1), keypair(2), keypair(3));
        for n in [None, Some(1), Some(3)] {
            let token = token_with_condition(
                p_s,
                vec![p_b, p_m],
                n,
                Some(LOCKTIME),
                Some(vec![p_s]),
                None,
            );
            assert!(
                CashuClient::verify_2of3_condition(&token, p_b, p_s, p_m).is_err(),
                "n_sigs {n:?} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_missing_locktime() {
        let (p_b, p_s, p_m) = (keypair(1), keypair(2), keypair(3));
        let token = token_with_condition(p_s, vec![p_b, p_m], Some(2), None, Some(vec![p_s]), None);
        assert!(CashuClient::verify_2of3_condition(&token, p_b, p_s, p_m).is_err());
    }

    #[test]
    fn rejects_missing_wrong_or_extra_refund_key() {
        let (p_b, p_s, p_m) = (keypair(1), keypair(2), keypair(3));
        // Missing refund tag: NUT-11 would make the token anyone-can-spend
        // after the locktime.
        let token = token_with_condition(p_s, vec![p_b, p_m], Some(2), Some(LOCKTIME), None, None);
        assert!(CashuClient::verify_2of3_condition(&token, p_b, p_s, p_m).is_err());
        // Wrong refund key (buyer instead of seller).
        let token = token_with_condition(
            p_s,
            vec![p_b, p_m],
            Some(2),
            Some(LOCKTIME),
            Some(vec![p_b]),
            None,
        );
        assert!(CashuClient::verify_2of3_condition(&token, p_b, p_s, p_m).is_err());
        // Extra refund key alongside the seller.
        let token = token_with_condition(
            p_s,
            vec![p_b, p_m],
            Some(2),
            Some(LOCKTIME),
            Some(vec![p_s, p_b]),
            None,
        );
        assert!(CashuClient::verify_2of3_condition(&token, p_b, p_s, p_m).is_err());
    }

    #[test]
    fn rejects_num_sigs_refund_other_than_one() {
        let (p_b, p_s, p_m) = (keypair(1), keypair(2), keypair(3));
        // cdk's own constructor refuses both shapes (ZeroSignaturesRequired
        // / ImpossibleRefundMultisigConfiguration) — but an attacker doesn't
        // use the constructor, they hand us a raw NUT-10 secret. Forge them
        // and make sure the daemon-side check still rejects the token (cdk
        // had a known refund-path zero-sigs bypass hazard).
        for n_sigs_refund in [0u64, 2] {
            let raw_secret = format!(
                r#"["P2PK",{{"nonce":"859d4935c4907062a6297cf4e663e2835d90d97ecdd510745d32f6816323a41f","data":"{}","tags":[["pubkeys","{}","{}"],["locktime","{LOCKTIME}"],["refund","{}"],["n_sigs","2"],["n_sigs_refund","{n_sigs_refund}"],["sigflag","SIG_INPUTS"]]}}]"#,
                p_s.to_hex(),
                p_b.to_hex(),
                p_m.to_hex(),
                p_s.to_hex(),
            );
            let token = raw_secret_token(&raw_secret);
            assert!(
                CashuClient::verify_2of3_condition(&token, p_b, p_s, p_m).is_err(),
                "a forged n_sigs_refund {n_sigs_refund} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_sig_all_escrow_token() {
        let (p_b, p_s, p_m) = (keypair(1), keypair(2), keypair(3));
        // A `SIG_ALL` escrow binds specific outputs and cannot be redeemed
        // by the documented SIG_INPUTS release/refund flow. cdk parses the
        // flag but `verify_2of3_condition` must reject it. Forge a raw
        // secret since `token_with_condition` hardcodes `SIG_INPUTS`.
        let raw_secret = format!(
            r#"["P2PK",{{"nonce":"859d4935c4907062a6297cf4e663e2835d90d97ecdd510745d32f6816323a41f","data":"{}","tags":[["pubkeys","{}","{}"],["locktime","{LOCKTIME}"],["refund","{}"],["n_sigs","2"],["sigflag","SIG_ALL"]]}}]"#,
            p_s.to_hex(),
            p_b.to_hex(),
            p_m.to_hex(),
            p_s.to_hex(),
        );
        let token = raw_secret_token(&raw_secret);
        assert!(
            CashuClient::verify_2of3_condition(&token, p_b, p_s, p_m).is_err(),
            "a SIG_ALL escrow token must be rejected"
        );
    }

    #[test]
    fn rejects_duplicate_standard_tags() {
        let (p_b, p_s, p_m) = (keypair(1), keypair(2), keypair(3));
        // cdk keeps the first `locktime` (valid) and drops the second, but a
        // non-cdk mint could honour the second (`1`, long past) instead —
        // NUT-11 marks duplicate tags malformed, so reject before parsing.
        let dup_locktime = format!(
            r#"["P2PK",{{"nonce":"859d4935c4907062a6297cf4e663e2835d90d97ecdd510745d32f6816323a41f","data":"{}","tags":[["pubkeys","{}","{}"],["locktime","{LOCKTIME}"],["locktime","1"],["refund","{}"],["n_sigs","2"],["sigflag","SIG_INPUTS"]]}}]"#,
            p_s.to_hex(),
            p_b.to_hex(),
            p_m.to_hex(),
            p_s.to_hex(),
        );
        let token = raw_secret_token(&dup_locktime);
        assert!(
            CashuClient::verify_2of3_condition(&token, p_b, p_s, p_m).is_err(),
            "a duplicate `locktime` tag must be rejected"
        );

        // A second `refund` tag pointing at the buyer: cdk keeps the seller
        // refund, but the malformed token must not be accepted at all.
        let dup_refund = format!(
            r#"["P2PK",{{"nonce":"859d4935c4907062a6297cf4e663e2835d90d97ecdd510745d32f6816323a41f","data":"{}","tags":[["pubkeys","{}","{}"],["locktime","{LOCKTIME}"],["refund","{}"],["refund","{}"],["n_sigs","2"],["sigflag","SIG_INPUTS"]]}}]"#,
            p_s.to_hex(),
            p_b.to_hex(),
            p_m.to_hex(),
            p_s.to_hex(),
            p_b.to_hex(),
        );
        let token = raw_secret_token(&dup_refund);
        assert!(
            CashuClient::verify_2of3_condition(&token, p_b, p_s, p_m).is_err(),
            "a duplicate `refund` tag must be rejected"
        );
    }

    #[test]
    fn rejects_missing_wrong_or_extra_pubkey() {
        let (p_b, p_s, p_m) = (keypair(1), keypair(2), keypair(3));
        let intruder = keypair(4);
        // Missing: only two keys in the set.
        let token = token_with_condition(
            p_s,
            vec![p_b],
            Some(2),
            Some(LOCKTIME),
            Some(vec![p_s]),
            None,
        );
        assert!(CashuClient::verify_2of3_condition(&token, p_b, p_s, p_m).is_err());
        // Wrong: intruder replaces Mostro's key.
        let token = token_with_condition(
            p_s,
            vec![p_b, intruder],
            Some(2),
            Some(LOCKTIME),
            Some(vec![p_s]),
            None,
        );
        assert!(CashuClient::verify_2of3_condition(&token, p_b, p_s, p_m).is_err());
        // Extra: a fourth key inflates the set.
        let token = token_with_condition(
            p_s,
            vec![p_b, p_m, intruder],
            Some(2),
            Some(LOCKTIME),
            Some(vec![p_s]),
            None,
        );
        assert!(CashuClient::verify_2of3_condition(&token, p_b, p_s, p_m).is_err());
        // Duplicate: p_m repeated to fake a 3-key set while only two
        // distinct keys can actually sign.
        let token = token_with_condition(
            p_s,
            vec![p_m, p_m],
            Some(2),
            Some(LOCKTIME),
            Some(vec![p_s]),
            None,
        );
        assert!(CashuClient::verify_2of3_condition(&token, p_b, p_s, p_m).is_err());
    }

    #[test]
    fn locktime_floor_is_enforced() {
        let (p_b, p_s, p_m) = (keypair(1), keypair(2), keypair(3));
        let token = valid_escrow_token(p_b, p_s, p_m);
        // At or above the floor: accepted (seller may set a longer one).
        assert!(verify_escrow_conditions(&token, p_b, p_s, p_m, LOCKTIME).is_ok());
        assert!(verify_escrow_conditions(&token, p_b, p_s, p_m, LOCKTIME - 1).is_ok());
        // Below the floor: a malicious seller could stall past a short
        // locktime and reclaim after the buyer sent fiat (Track A §4B).
        assert!(verify_escrow_conditions(&token, p_b, p_s, p_m, LOCKTIME + 1).is_err());
    }

    #[test]
    fn rejects_duplicate_proofs_in_token() {
        let proof = Proof {
            keyset_id: Id::from_str(TEST_KEYSET_ID).unwrap(),
            amount: Amount::from(50u64),
            secret: Secret::generate(),
            c: PublicKey::from_str(TEST_C).unwrap(),
            witness: None,
            dleq: None,
            p2pk_e: None,
        };
        // The same proof twice: `value()` would double-count to 100 and
        // checkstate would report each copy unspent, so a 50-sat proof could
        // satisfy a 100-sat escrow. `reject_duplicate_proofs` must catch it.
        let dup = Token::new(
            MintUrl::from_str(TEST_MINT).unwrap(),
            vec![proof.clone(), proof],
            None,
            CurrencyUnit::Sat,
        );
        assert!(reject_duplicate_proofs(&dup).is_err());

        // Two distinct proofs (different secrets) pass.
        let mk = |amount: u64| Proof {
            keyset_id: Id::from_str(TEST_KEYSET_ID).unwrap(),
            amount: Amount::from(amount),
            secret: Secret::generate(),
            c: PublicKey::from_str(TEST_C).unwrap(),
            witness: None,
            dleq: None,
            p2pk_e: None,
        };
        let distinct = Token::new(
            MintUrl::from_str(TEST_MINT).unwrap(),
            vec![mk(50), mk(50)],
            None,
            CurrencyUnit::Sat,
        );
        assert!(reject_duplicate_proofs(&distinct).is_ok());
    }

    #[test]
    fn xonly_to_cashu_pubkey_prepends_even_parity() {
        let pk = cashu_pubkey_from_xonly_hex(XONLY_HEX).expect("valid x-only converts");
        // The compressed form is the even-parity (0x02) point over the x-only.
        assert_eq!(pk.to_hex(), format!("02{XONLY_HEX}"));
    }

    #[test]
    fn xonly_to_cashu_pubkey_rejects_wrong_length() {
        // 63 chars (too short) and the already-prefixed 66-char form must
        // both be rejected: the helper expects exactly the 64-char x-only.
        assert!(cashu_pubkey_from_xonly_hex(&XONLY_HEX[..63]).is_err());
        assert!(cashu_pubkey_from_xonly_hex(&format!("02{XONLY_HEX}")).is_err());
    }

    #[test]
    fn xonly_to_cashu_pubkey_rejects_non_hex() {
        // Right length, but not valid hex / not a curve point.
        let not_hex = "z".repeat(64);
        assert!(cashu_pubkey_from_xonly_hex(&not_hex).is_err());
    }

    /// The derivation always prepends `02`, but a Nostr trade key's point
    /// may have ODD-Y parity (`03`). NUT-11 P2PK uses BIP340 Schnorr, which
    /// verifies against the x-only coordinate (the even-Y lift), so a
    /// signature by such a key must still validate against the
    /// `02`-derived pubkey. If this failed, the parity handling would be
    /// wrong and Track B (where the buyer redeems with trade-key
    /// signatures) would break. This is the sign/verify roundtrip proving
    /// it.
    #[test]
    fn odd_y_nostr_key_signs_for_derived_cashu_pubkey() {
        use cdk::nuts::nut00::Witness;
        use cdk::nuts::nut11::P2PKWitness;
        use nostr_sdk::Keys;

        // Find a Nostr key whose secp256k1 point has odd-Y parity (`0x03`).
        let (keys, nut_sk) = loop {
            let keys = Keys::generate();
            let nut_sk = NutSecretKey::from_hex(keys.secret_key().to_secret_hex())
                .expect("nostr secret converts to a cdk secret key");
            if nut_sk.public_key().to_bytes()[0] == 0x03 {
                break (keys, nut_sk);
            }
        };

        let xonly_hex = keys.public_key().to_hex();
        let cashu_pk = cashu_pubkey_from_xonly_hex(&xonly_hex).expect("derive cashu pubkey");
        // Derivation forced even-Y even though the source key is odd-Y.
        assert_eq!(cashu_pk.to_bytes()[0], 0x02);

        // Lock a proof to a 1-of-1 P2PK over the derived pubkey, sign with
        // the odd-Y Nostr key, and verify.
        let secret: Secret = SpendingConditions::P2PKConditions {
            data: cashu_pk,
            conditions: None,
        }
        .try_into()
        .expect("p2pk secret");
        let mut proof = Proof {
            keyset_id: Id::from_str(TEST_KEYSET_ID).unwrap(),
            amount: Amount::ZERO,
            secret,
            c: PublicKey::from_str(TEST_C).unwrap(),
            witness: Some(Witness::P2PKWitness(P2PKWitness { signatures: vec![] })),
            dleq: None,
            p2pk_e: None,
        };
        proof.sign_p2pk(nut_sk).expect("sign with the odd-Y key");
        assert!(
            proof.verify_p2pk().is_ok(),
            "an odd-Y Nostr key must sign validly for the 02-derived cashu pubkey"
        );
    }

    /// Mint-authentication (step 4 of `verify_escrow_token` →
    /// `verify_token_dleq` → `Proof::verify_dleq`) closes the
    /// fabricated-token hole only because cdk rejects a proof carrying NO
    /// DLEQ. Pin that pinned-dependency behavior so a future cdk bump can't
    /// silently reopen it: an absent DLEQ must error with
    /// `MissingDleqProof`. (The full `verify_escrow_token` path needs a
    /// live mint and is covered by the env-gated CF-3 integration suite;
    /// this is the offline regression guard for the exact attack
    /// primitive.)
    #[test]
    fn proof_without_dleq_is_rejected() {
        let proof = Proof {
            keyset_id: Id::from_str(TEST_KEYSET_ID).unwrap(),
            amount: Amount::ZERO,
            secret: Secret::generate(),
            c: PublicKey::from_str(TEST_C).unwrap(),
            witness: None,
            dleq: None,
            p2pk_e: None,
        };
        let mint_pubkey = PublicKey::from_str(TEST_C).unwrap();
        assert!(
            matches!(
                proof.verify_dleq(mint_pubkey),
                Err(cdk::nuts::nut12::Error::MissingDleqProof)
            ),
            "a proof with no DLEQ must be rejected (fabricated-token defense)"
        );
    }
}
