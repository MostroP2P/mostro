# Repository Guidelines

## Project Structure & Module Organization
- Runtime code lives in `src/`.
  - `src/app/` – order flows and business logic.
  - `src/lightning/` – LND bindings and Lightning helpers.
  - `src/rpc/` – gRPC service and types.
  - `src/config/` – settings and loaders.
- DB migrations in `migrations/`. Reusable assets in `static/`.
- SQLx offline metadata in `sqlx-data.json`.
- Docker assets in `docker/`. Cross‑compile targets in `archs/`.

## Build, Test, and Development Commands
- `cargo build` – compile daemon (debug).
- `cargo run` – start Mostro using current `settings.toml`.
- `cargo test` – run module tests; keep green before pushing.
- `cargo fmt` – apply Rustfmt profile.
- `cargo clippy --all-targets --all-features` – lints must be clean.
- `make docker-up` / `make docker-down` – start/stop local relay stack.
- After SQL/schema changes: `cargo sqlx prepare -- --bin mostrod` to refresh `sqlx-data.json`.

## Coding Style & Naming Conventions
- Rust 2021: 4‑space indent, `snake_case` functions, `PascalCase` types, `SCREAMING_SNAKE` constants.
- Document non‑obvious public APIs with `///`.
- Prefer `tracing` spans over ad‑hoc logging.
- Keep config templates in `settings.tpl.toml`.

## Testing Guidelines
- Co‑locate tests in their modules under `mod tests`.
- Name descriptively, e.g., `handles_expired_hold_invoice`.
- Mirror fixtures under `src/app/` where applicable.
- Run `cargo test` locally; update SQLx data after query changes.

## Commit & Pull Request Guidelines
- Base work on `main`; keep topics scoped.
- Commit subject: imperative, ≤50 chars; sign with `git commit -S`.
- Squash fixups before review.
- PRs: link the motivating issue, include `cargo test` output, and call out schema or config changes to ease verification.

## Security & Configuration Tips
- Do not commit populated `settings.toml`. Copy from `settings.tpl.toml` to `~/.mostro/settings.toml` for local runs.
- Protect LND credentials before `make docker-build`.
- Scrub logs that might leak invoices or Nostr keys; rotate secrets promptly if exposed.
