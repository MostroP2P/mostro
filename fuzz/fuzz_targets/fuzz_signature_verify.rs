//! Fuzz target: Message signature verification
//!
//! Tests that `Message::verify_signature()` handles arbitrary message
//! strings, public keys, and signatures without panicking.
//!
//! Related issue: https://github.com/MostroP2P/mostro/issues/592

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use mostro_core::message::Message;
use nostr_sdk::prelude::*;

#[derive(Arbitrary, Debug)]
struct FuzzSigInput {
    message: Vec<u8>,
    pubkey_bytes: [u8; 32],
    sig_bytes: [u8; 64],
}

fuzz_target!(|input: FuzzSigInput| {
    let msg_str = String::from_utf8_lossy(&input.message).to_string();

    // Try to construct a valid PublicKey from the fuzzed bytes
    let pubkey = match PublicKey::from_slice(&input.pubkey_bytes) {
        Ok(pk) => pk,
        Err(_) => return,
    };

    // Try to construct a Signature from the fuzzed bytes
    let sig = match nostr_sdk::secp256k1::schnorr::Signature::from_slice(&input.sig_bytes) {
        Ok(s) => s,
        Err(_) => return,
    };

    // verify_signature must never panic, only return true/false
    let _ = Message::verify_signature(msg_str, pubkey, sig);
});
