# Repository Guidelines

## Project Structure & Module Organization
Mostro is a Rust daemon; the `src/` directory holds the runtime modules (`app/` for order flows, `lightning/` for LND integration, `rpc/` for the gRPC server, and `config/` for settings helpers). SQL migrations live in `migrations/`, prepared query metadata in `sqlx-data.json`, and reusable assets under `static/`. Packaging and deployment resources are in `docker/`, while `archs` records supported cross-compilation targets.

## Build, Test, and Development Commands
Use `cargo build` for a debug build and `cargo run` to start the daemon with the current `settings.toml`. Execute `cargo test` before opening a pull request; it covers the module-level `#[cfg(test)]` suites. `cargo fmt` enforces the Rustfmt profile, and `cargo clippy --all-targets --all-features` catches lints. For local relay + services, run `make docker-up`; tear it down with `make docker-down`. When modifying SQLx queries, regenerate offline data with `cargo sqlx prepare -- --bin mostrod` so `sqlx-data.json` stays in sync.

## Coding Style & Naming Conventions
Follow Rust 2021 defaults: four-space indentation, `snake_case` modules/functions, `PascalCase` types, and screaming snake constants. Keep public APIs documented with `///` comments when behavior is non-obvious, and prefer `tracing` spans for new instrumentation. Always stage formatting and lint fixes via `cargo fmt && cargo clippy` before committing.

## Testing Guidelines
Co-locate new tests inside the module they exercise, using `mod tests` and descriptive function names (e.g., `handles_expired_hold_invoice`). Mimic existing fixtures in `src/app/*` when mocking order flows. Update `sqlx-data.json` whenever a query signature or schema changes to keep CI from failing.

## Commit & Pull Request Guidelines
Base work on `main` and keep branches focused. Commits should use imperative, ≤50-character subjects, contain a wrapped body explaining rationale, and be GPG-signed (`git commit -S`). Squash fixups before review; avoid merge commits. Pull requests must link the motivating issue, list manual test output (e.g., `cargo test`), and call out config or schema migrations so reviewers can test upgrades.

## Security & Configuration Tips
Do not commit populated `settings.toml`; use `settings.tpl.toml` as the template and keep private keys outside version control. Running `cargo run` copies the config into `~/.mostro`; mirror manual edits there. Protect LND credentials before invoking `make docker-build`, and scrub any logs that might reveal invoices or nostr keys prior to sharing.
