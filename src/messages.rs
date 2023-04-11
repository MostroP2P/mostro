use anyhow::Result;
use nostr_sdk::prelude::*;
use uuid::Uuid;

pub fn cant_do() -> String {
    "You can't do that!".to_string()
}

pub fn hold_invoice_description(
    mostro_pubkey: XOnlyPublicKey,
    order_id: &str,
    fiat_code: &str,
    fiat_amount: &str,
) -> Result<String> {
    Ok(format!(
        "{} - Escrow amount Order #{order_id}: SELL BTC for {fiat_code} {fiat_amount} - It WILL FREEZE IN WALLET. It will release once you release. It will return if buyer does not confirm the payment", mostro_pubkey.to_bech32()?
    ))
}
