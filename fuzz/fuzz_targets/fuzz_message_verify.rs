//! Fuzz target: MessageKind::verify() — action/payload matching validation
//!
//! Tests that the `verify()` method on `Message` correctly validates
//! action/payload combinations without panicking, even with arbitrary
//! JSON that deserializes to valid (but nonsensical) messages.
//!
//! Related issue: https://github.com/MostroP2P/mostro/issues/591

#![no_main]

use libfuzzer_sys::fuzz_target;
use mostro_core::message::Message;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        if let Ok(msg) = serde_json::from_str::<Message>(s) {
            // Exercise the verify path — checks action/payload consistency
            let _ = msg.verify();

            // Exercise inner accessors — must not panic
            let _ = msg.inner_action();
            let kind = msg.get_inner_message_kind();
            let _ = kind.get_action();
            let _ = kind.get_rating();
            let _ = kind.get_next_trade_key();

            // Roundtrip: serialize back and re-parse
            if let Ok(json) = msg.as_json() {
                let _ = Message::from_json(&json);
            }
        }
    }
});
