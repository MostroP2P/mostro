# Fuzz Testing

Mostro uses [cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html) (libFuzzer backend) to test critical input parsing and validation paths against malformed or adversarial data.

## Prerequisites

```bash
# Install cargo-fuzz
cargo install cargo-fuzz

# Nightly toolchain is required
rustup install nightly
```

## Quick Start

```bash
# List all fuzz targets
cargo +nightly fuzz list

# Run a specific target (runs until interrupted with Ctrl+C)
cargo +nightly fuzz run fuzz_message_deser

# Run for a limited time (e.g., 60 seconds)
cargo +nightly fuzz run fuzz_message_deser -- -max_total_time=60

# Run all targets for 30 seconds each (smoke test)
for target in $(cargo +nightly fuzz list); do
  echo "=== $target ==="
  cargo +nightly fuzz run "$target" -- -max_total_time=30
done
```

## Fuzz Targets

| Target | Description | Priority | Issues |
|--------|-------------|----------|--------|
| `fuzz_message_deser` | `Message` JSON deserialization | 🔴 High | #588 |
| `fuzz_message_tuple` | `(Message, Option<Signature>)` tuple — exact network entry point | 🔴 High | #588, #592 |
| `fuzz_order_deser` | `SmallOrder` JSON deserialization + roundtrip | 🔴 High | #589 |
| `fuzz_bolt11_decode` | BOLT11 invoice string parsing | 🔴 High | #590 |
| `fuzz_lnurl_parse` | LNURL and Lightning address format parsing | 🔴 High | #590 |
| `fuzz_message_verify` | `MessageKind::verify()` action/payload validation | Medium | #591 |
| `fuzz_signature_verify` | `Message::verify_signature()` with arbitrary keys/sigs | Medium | #592 |
| `fuzz_settings_toml` | TOML configuration parsing | Low | #593 |

## Seed Corpus

Each target has handcrafted seed inputs in `fuzz/seeds/<target>/` to bootstrap the fuzzer with valid examples. The fuzzer mutates these seeds to explore new code paths. The auto-generated corpus in `fuzz/corpus/` is gitignored.

To run with seeds:

```bash
# The fuzzer automatically picks up seeds from the corpus directory.
# Copy seeds before running:
cp -r fuzz/seeds/fuzz_message_deser/* fuzz/corpus/fuzz_message_deser/

# Or run directly with seeds directory as extra argument:
cargo +nightly fuzz run fuzz_message_deser fuzz/seeds/fuzz_message_deser
```

To add new seeds:

```bash
echo -n '{"order":{"version":1,...}}' > fuzz/seeds/fuzz_message_deser/my_new_seed.json
```

## Investigating Crashes

When the fuzzer finds a crash, it saves the input to `fuzz/artifacts/<target>/`:

```bash
# Reproduce a crash
cargo +nightly fuzz run fuzz_message_deser fuzz/artifacts/fuzz_message_deser/crash-abc123

# Minimize the crashing input
cargo +nightly fuzz tmin fuzz_message_deser fuzz/artifacts/fuzz_message_deser/crash-abc123
```

## Writing New Targets

1. Create a new file in `fuzz/fuzz_targets/`
2. Add a `[[bin]]` entry in `fuzz/Cargo.toml`
3. Add seed corpus files in `fuzz/corpus/<target_name>/`

Template:

```rust
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        // Your parsing/validation code here — must never panic
        let _ = your_function(s);
    }
});
```

## References

- [Rust Fuzz Book](https://rust-fuzz.github.io/book/)
- [cargo-fuzz docs](https://rust-fuzz.github.io/book/cargo-fuzz.html)
- [Milestone: Fuzz Testing & Input Hardening](https://github.com/MostroP2P/mostro/milestone/4)
