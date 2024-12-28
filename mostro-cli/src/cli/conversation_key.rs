use anyhow::Result;
use nip44::v2::ConversationKey;
use nostr_sdk::prelude::*;

pub async fn execute_conversation_key(trade_keys: &Keys, receiver: PublicKey) -> Result<()> {
    // Derive conversation key
    let ck = ConversationKey::derive(trade_keys.secret_key(), &receiver);
    let key = ck.as_bytes();
    let mut ck_hex = vec![];
    for i in key {
        ck_hex.push(format!("{:02x}", i));
    }
    let ck_hex = ck_hex.join("");
    println!("Conversation key: {:?}", ck_hex);

    Ok(())
}
