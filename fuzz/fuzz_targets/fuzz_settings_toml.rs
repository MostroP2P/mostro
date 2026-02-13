//! Fuzz target: TOML configuration parsing
//!
//! Tests that arbitrary TOML input can be parsed without panicking.
//! Uses toml::Value as the deserialization target since the Settings
//! struct requires runtime initialization. This still exercises the
//! TOML parser which is the first line of defense against malformed configs.
//!
//! Related issue: https://github.com/MostroP2P/mostro/issues/593

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        // Parse as generic TOML value — must never panic
        let _ = toml::from_str::<toml::Value>(s);

        // Also try deserializing as a Table specifically
        let _ = toml::from_str::<toml::map::Map<String, toml::Value>>(s);
    }
});
