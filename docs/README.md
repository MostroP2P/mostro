# Mostro Documentation

Quick links to architecture and feature guides.

- Architecture Overview: docs/ARCHITECTURE.md
- Startup & Configuration: docs/STARTUP_AND_CONFIG.md
- Event Routing: docs/EVENT_ROUTING.md
- Lightning Operations: docs/LIGHTNING_OPS.md
- Orders & Actions: docs/ORDERS_AND_ACTIONS.md
- Admin RPC & Disputes: docs/ADMIN_RPC_AND_DISPUTES.md
- RPC Interface Reference: docs/RPC.md

Tips
- Run tests and lints before pushing: `cargo test`, `cargo fmt`, `cargo clippy --all-targets --all-features`.
- Update SQLx offline data after query/schema changes: `cargo sqlx prepare -- --bin mostrod`.
