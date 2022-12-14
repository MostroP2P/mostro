use nostr::util::nips::nip04::decrypt;
use nostr::Event;
use nostr_sdk::nostr::Keys;

use crate::types;

pub fn handle_dm(keys: &Keys, event: &Event) {
    if let Ok(msg) = decrypt(&keys.secret_key().unwrap(), &event.pubkey, &event.content) {
        println!("New DM: {}", msg);
        let message = types::Message::from_json(&msg);
        if let Ok(msg) = message {
            if msg.verify() {
                println!(
                    "User with pubkey {} sent this valid message: {:#?}",
                    event.pubkey, msg,
                );
            }
        }
    } else {
        log::error!("Impossible to decrypt direct message");
    }
}
