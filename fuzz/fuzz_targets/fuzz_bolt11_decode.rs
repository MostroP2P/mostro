//! Fuzz target: BOLT11 Lightning invoice decoding
//!
//! Tests that BOLT11 invoice parsing handles arbitrary strings without
//! panicking. Exercises both the high-level Bolt11Invoice parser and
//! the raw SignedRawBolt11Invoice parser.
//!
//! Related issue: https://github.com/MostroP2P/mostro/issues/590

#![no_main]

use libfuzzer_sys::fuzz_target;
use lightning_invoice::{Bolt11Invoice, SignedRawBolt11Invoice};
use std::str::FromStr;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        // Bolt11Invoice::from_str must never panic on any input
        let _ = Bolt11Invoice::from_str(s);

        // Also test the raw invoice parsing path used in validate_bolt11_invoice
        let _ = s.parse::<SignedRawBolt11Invoice>();
    }
});
