//! Fuzz target: MostroMessage deserialization from untrusted JSON
//!
//! Tests that `Message::from_json()` handles arbitrary byte input without
//! panicking. This is the primary entry point for untrusted data from the
//! Nostr network.
//!
//! Related issue: https://github.com/MostroP2P/mostro/issues/588

#![no_main]

use libfuzzer_sys::fuzz_target;
use mostro_core::message::Message;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        // Try deserializing as a Message — must never panic
        let _ = Message::from_json(s);

        // Also try deserializing individual MessageKind
        let _ = serde_json::from_str::<mostro_core::message::MessageKind>(s);
    }
});
