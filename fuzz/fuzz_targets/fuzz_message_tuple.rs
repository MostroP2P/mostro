//! Fuzz target: (Message, Option<Signature>) tuple deserialization
//!
//! This is the exact format mostrod parses from NIP-59 gift wrap rumor
//! content in `src/app.rs`. Any panic here is directly exploitable by
//! any Nostr user sending a malformed gift wrap event.
//!
//! Related issues:
//! - https://github.com/MostroP2P/mostro/issues/588
//! - https://github.com/MostroP2P/mostro/issues/592

#![no_main]

use libfuzzer_sys::fuzz_target;
use mostro_core::message::Message;
use nostr_sdk::secp256k1::schnorr::Signature;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        // The exact deserialization path from src/app.rs ~line 334
        let _ = serde_json::from_str::<(Message, Option<Signature>)>(s);

        // Also try the non-optional variant (src/app.rs ~line 135)
        let _ = serde_json::from_str::<(Message, Signature)>(s);
    }
});
