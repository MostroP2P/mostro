//! Fuzz target: SmallOrder deserialization and field validation
//!
//! Tests that `SmallOrder` deserialization from arbitrary JSON handles
//! edge cases in numeric fields (amount, min/max, fiat_amount, premium)
//! and string fields (fiat_code, payment_method) without panicking.
//!
//! Related issue: https://github.com/MostroP2P/mostro/issues/589

#![no_main]

use libfuzzer_sys::fuzz_target;
use mostro_core::order::SmallOrder;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        // Deserialize SmallOrder from arbitrary JSON
        if let Ok(order) = serde_json::from_str::<SmallOrder>(s) {
            // Exercise serialization roundtrip — must not panic
            let _ = serde_json::to_string(&order);

            // Exercise the as_json / from_json path
            if let Ok(json) = order.as_json() {
                let _ = SmallOrder::from_json(&json);
            }
        }
    }
});
