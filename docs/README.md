# Mostro Documentation

Quick links to architecture and feature guides.

- Architecture Overview: ARCHITECTURE.md
- Startup & Configuration: STARTUP_AND_CONFIG.md (template keys are required; see notes on Rust Defaults)
- Event Routing: EVENT_ROUTING.md
- Lightning Operations: LIGHTNING_OPS.md
- Orders & Actions: ORDERS_AND_ACTIONS.md
- Admin RPC & Disputes: ADMIN_RPC_AND_DISPUTES.md
- RPC Interface Reference: RPC.md

Tips
- Run tests and lints before pushing: `cargo test`, `cargo fmt`, `cargo clippy --all-targets --all-features`.
- Update SQLx offline data after query/schema changes: `cargo sqlx prepare -- --bin mostrod`.
