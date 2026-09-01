# Mostro Documentation

Quick links to architecture and feature guides.

- Architecture Overview: ARCHITECTURE.md
- Startup & Configuration: STARTUP_AND_CONFIG.md (template keys are required; see notes on Rust Defaults)
- Event Routing: EVENT_ROUTING.md
- Lightning Operations: LIGHTNING_OPS.md
- Orders & Actions: ORDERS_AND_ACTIONS.md
- Admin RPC & Disputes: ADMIN_RPC_AND_DISPUTES.md
- Anti-Abuse Bond: ANTI_ABUSE_BOND.md (opt-in maker/taker Lightning bond; off by default)
- Payment-Account History / Anti-Triangulation: PAYER_HISTORY_ANTI_TRIANGULATION.md (opt-in buyer payer declaration + private success history; off by default)
- Maintenance Mode & LN Node Migration: MAINTENANCE_MODE_LN_MIGRATION.md (drain mode spec: block new/take, drain escrow, switch node)
- RPC Interface Reference: RPC.md
- NIP-01 Kind 0 Metadata: NIP01_KIND0_METADATA.md

Tips
- Run tests and lints before pushing: `cargo test`, `cargo fmt`, `cargo clippy --all-targets --all-features`.
- After schema changes: add a migration under `migrations/`; run `sqlx migrate run` if needed (or let `mostrod` migrate on connect).

- [Solver Permission Levels](./SOLVER_PERMISSION_LEVELS.md)
