# Mutation Testing Implementation for Mostro

## Overview

This document describes the mutation testing implementation for Mostro, a critical Rust daemon handling Bitcoin P2P trades over Lightning Network and Nostr.

## What is Mutation Testing?

Mutation testing is a technique to measure the quality and effectiveness of the test suite. Unlike code coverage (which only checks if code is executed), mutation testing verifies that tests actually detect bugs.

### How it works:

1. **Creating mutants**: The tool makes small, controlled changes (mutations) to the source code
   - Change `==` to `!=`, `+` to `-`, `true` to `false`
   - Remove a line of code, change a condition boundary
   - Replace `&&` with `||`, `>` with `>=`
   - Replace function return value with default

2. **Running the test suite**: Tests are executed against each mutated version

3. **Measuring survival rate**:
   - ✅ **Mutant killed**: Test failed → Good! Tests detected the artificial bug
   - ❌ **Mutant survived**: Test passed → Bad! Tests did not catch the change

### Mutation Score

```
Score = (Mutants killed / Total mutants) × 100
```

- **> 80%**: Excellent test quality
- **50-80%**: Acceptable, room for improvement
- **< 50%**: Poor test quality, needs immediate attention

## Why Mutation Testing for Mostro?

1. **Financial security**: Trading logic, escrow handling, and dispute resolution must be bulletproof
2. **Detect weak tests**: Find tests that "cover" code but don't actually verify correctness
3. **Force better assertions**: Encourages specific, strict assertions instead of generic ones
4. **Find edge cases**: Surviving mutants often reveal untested boundary conditions
5. **CI integration**: Can fail builds if mutation score drops below threshold
6. **Documentation**: Living documentation of what behavior is actually tested
7. **Refactoring safety**: High mutation score gives confidence when refactoring critical code

## Tool Selection: cargo-mutants

We use `cargo-mutants` as it is the most mature mutation testing tool for Rust.

### Installation

```bash
cargo install cargo-mutants
```

### Basic Usage

```bash
# Test all mutants
cargo mutants

# Test with specific packages
cargo mutants -p mostro-core

# Generate HTML report
cargo mutants --html

# Test specific files
cargo mutants --file src/flow.rs

# Test with sharding (for CI parallelization)
cargo mutants --shard 1/4
```

## Configuration

The `.mutants.toml` file configures mutation testing behavior:

```toml
# Examine only source files
examine_globs = [
    "src/**/*.rs",
]

# Exclude generated code, tests, and non-critical modules
exclude_globs = [
    "**/target/**",
    "**/tests/**",
    "**/examples/**",
    "src/main.rs",           # Entry point, minimal logic
    "src/cli.rs",            # CLI parsing
    "src/config/**",         # Configuration loading
    "src/scheduler.rs",      # Background jobs
    "src/bitcoin_price.rs",  # External API calls
    "src/rpc/**",            # RPC server
]

# Timeout for each mutant (seconds)
# Mostro tests involve DB operations, so we need generous timeout
timeout = 600

# Number of parallel jobs
jobs = 4

# Output directory
output_dir = "mutants.out"

# Additional cargo test arguments
test_tool_options = ["--", "--test-threads=1"]
```

## CI/CD Integration

Mutation testing runs in CI on every PR and on main branch:

1. **PR workflow**: Runs mutation testing on changed files only (faster feedback)
2. **Main branch**: Runs full mutation testing weekly (baseline tracking)
3. **Release gate**: Mutation score must not decrease from previous release

### Initial Setup (Non-blocking)

Initially, mutation testing runs in "report only" mode:
- Results are uploaded as artifacts
- CI does NOT fail on low mutation score
- Team reviews results and improves tests incrementally

### Phase 2 (Enforcing)

Once baseline is established:
- CI fails if mutation score drops below threshold
- New code must maintain or improve mutation score

## Priority Areas

Critical modules that should have high mutation scores (>80%):

| Module | Priority | Rationale |
|--------|----------|-----------|
| `src/flow.rs` | Critical | Order state transitions, trade logic |
| `src/db.rs` | Critical | Database operations, state persistence |
| `src/util.rs` | Critical | Utility functions used across codebase |
| `src/nip33.rs` | High | Nostr event tagging |
| `src/lnurl.rs` | High | LNURL handling |
| `src/messages.rs` | Medium | Message formatting |
| `src/models.rs` | Medium | Data models |

## Implementation Phases

### Phase 1: Infrastructure (This PR)

- [x] Install and configure cargo-mutants
- [x] Create `.mutants.toml` configuration
- [x] Add mutation testing CI workflow
- [x] Document the strategy

### Phase 2: Baseline Assessment

- [ ] Run full mutation testing on current codebase
- [ ] Document current mutation score per module
- [ ] Identify "survivors" (mutants that passed tests)
- [ ] Prioritize critical modules

### Phase 3: Critical Module Improvements

- [ ] Improve tests for `src/flow.rs` (target: 80%)
- [ ] Improve tests for `src/db.rs` (target: 80%)
- [ ] Improve tests for `src/util.rs` (target: 80%)

### Phase 4: Enable Enforcement

- [ ] Set minimum mutation score threshold in CI
- [ ] Fail builds if score drops
- [ ] Track score trends over time

## Interpreting Results

### Example Output

```
INFO Found 245 mutants to test
INFO 189 mutants killed (77.1%)
INFO 56 mutants survived (22.9%)
INFO 0 mutants timed out
INFO Mutation score: 77.1%
```

### Analyzing Survivors

Each surviving mutant represents a potential gap in testing:

```
INFO src/flow.rs:245:9: replace Order::validate -> bool with true
```

This mutant replaced the `validate` method with `return true`, and tests still passed. This means:
- Either the validation logic is not tested
- Or tests don't verify validation failures

### Fixing Survivors

Add tests that would catch the mutation:

```rust
#[test]
fn test_order_validation_rejects_invalid() {
    let order = Order::new(/* invalid data */);
    assert!(!order.validate()); // This would catch the mutant
}
```

## Running Locally

```bash
# Quick check (mutants only in changed files)
cargo mutants --in-diff HEAD~1

# Full run (takes ~30-60 min)
cargo mutants

# Specific module
cargo mutants --file src/flow.rs

# Generate HTML report
cargo mutants --html
open mutants.out/index.html
```

## Performance Considerations

Mutation testing is computationally expensive:

- Full run: ~30-60 minutes depending on hardware
- Each mutant requires a full test suite run
- Use sharding for parallelization in CI
- Start with critical modules only

## Troubleshooting

### Timeout Issues

If mutants timeout, increase the timeout in `.mutants.toml`:

```toml
timeout = 900  # 15 minutes
```

### False Positives

Some mutations may be equivalent (changing code that doesn't affect behavior). Add to exclude list:

```toml
exclude_re = [
    "replace .*::default\\(\\) -> Self with",  # Default impls often equivalent
]
```

### Build Failures

If mutation causes compilation errors (not test failures), cargo-mutants should handle this automatically. If not:

```toml
# Exclude files with heavy macros
exclude_globs = [
    "src/proto/**",
]
```

## References

- [cargo-mutants documentation](https://mutants.rs/)
- [Mutation Testing Wikipedia](https://en.wikipedia.org/wiki/Mutation_testing)
- [Mostro Protocol Specification](https://mostro.network/protocol/)

## Checklist for This Implementation

- [x] `.mutants.toml` created with appropriate configuration
- [x] CI workflow added for mutation testing
- [x] Documentation written (`docs/MUTATION_TESTING.md`)
- [x] Non-blocking in CI (report-only mode initially)
- [ ] Baseline mutation score documented (Phase 2)
- [ ] Tests improved for critical survivors (Phase 3)
