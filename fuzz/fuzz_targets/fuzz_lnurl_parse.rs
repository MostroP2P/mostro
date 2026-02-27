//! Fuzz target: LNURL and Lightning address parsing
//!
//! Tests the parsing logic for LNURL strings and Lightning addresses
//! without making any network requests. Focuses on the format detection
//! and parsing that happens before network validation.
//!
//! Related issue: https://github.com/MostroP2P/mostro/issues/590

#![no_main]

use libfuzzer_sys::fuzz_target;
use std::str::FromStr;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        // Test Lightning address parsing (email-like format)
        let _ = lnurl::lightning_address::LightningAddress::from_str(s);

        // Test LNURL parsing and decoding
        let _ = lnurl::lnurl::LnUrl::from_str(s);

        // Test the LNURL decode path
        let _ = lnurl::lnurl::LnUrl::decode(s.to_string());

        // Test the split logic used in extract_lnurl
        if let Some((user, domain)) = s.split_once('@') {
            // Exercise the URL construction path — must not panic
            let _url = format!("https://{}/.well-known/lnurlp/{}", domain, user);
        }
    }
});
