# Repository Guidelines

## Project Structure & Module Organization
Mostro is a Rust daemon; runtime code lives under `src/`, with `app/` for order flows, `lightning/` for LND bindings, `rpc/` for the gRPC surface, and `config/` for settings helpers. Database migrations reside in `migrations/`, reusable assets in `static/`, and SQLx offline metadata in `sqlx-data.json`. Docker assets inside `docker/` drive local infrastructure; cross-compilation targets are tracked in `archs/`.

## Build, Test, and Development Commands
- `cargo build` compiles the daemon in debug mode.
- `cargo run` launches Mostro using the current `settings.toml`.
- `cargo test` exercises module-level suites; run it before pushing.
- `cargo fmt` enforces the Rustfmt profile; `cargo clippy --all-targets --all-features` must be clean.
- `make docker-up` boots the relay stack; shut it down with `make docker-down` when finished.

## Coding Style & Naming Conventions
Use Rust 2021 defaults: four-space indentation, `snake_case` functions, `PascalCase` types, and screaming snake constants. Document non-obvious public APIs with `///`. Favor `tracing` spans over ad-hoc logging, and keep configuration templates in `settings.tpl.toml`.

## Testing Guidelines
Co-locate tests within their modules inside `mod tests`. Name cases descriptively (e.g., `handles_expired_hold_invoice`) and mirror fixtures already present under `src/app/`. After changing SQLx queries or schema, regenerate offline data with `cargo sqlx prepare -- --bin mostrod` so `sqlx-data.json` stays valid.

## Commit & Pull Request Guidelines
Base work on `main`, keep topics scoped, and write imperative commit subjects ≤50 characters. Sign commits with `git commit -S` and squash fixups before review. Pull requests should link the motivating issue, list manual test output (e.g., `cargo test`), and call out schema or config changes to ease verification.

## Security & Configuration Tips
Never commit populated `settings.toml`; copy from `settings.tpl.toml` into `~/.mostro/settings.toml` when running locally. Protect LND credentials before `make docker-build`, and scrub logs that might leak invoices or nostr keys. Rotate secrets promptly if accidents happen.
