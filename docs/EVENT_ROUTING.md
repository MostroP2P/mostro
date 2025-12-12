# Event Routing

How Nostr events become actions and side effects.

## Intake Pipeline
- Source: `src/app.rs:run`
- Steps: POW check → signature verify → recency guard → NIP-59 unwrap → parse `mostro_core::Message` → inner verify → `check_trade_index` → dispatch.

## Dispatch
- Router: `src/app.rs:handle_message_action`
- Maps `Action` → module function under `src/app/*`.
- On `MostroError`, `manage_errors` pushes user-facing “can’t do” messages or logs warnings.

## Trade Index
- Function: `src/app.rs:check_trade_index`
- Ensures monotonic `trade_index` for trading actions; verifies signature binding; auto-creates user on first valid trade.

## Key Actions (entries)
- Take Buy: `src/app/take_buy.rs:12`
- Add Invoice: `src/app/add_invoice.rs:34`
- Release: `src/app/release.rs:160`
- Cancel: `src/app/cancel.rs`
- Dispute: `src/app/dispute.rs`

## Diagram
```mermaid
sequenceDiagram
  participant Relay as Nostr Relay
  participant Loop as app.rs (run)
  participant Router as handle_message_action
  participant Mod as app/*
  participant DB as DB
  participant LND as LND

  Relay-->>Loop: GiftWrap Event
  Loop->>Loop: POW + verify + freshness
  Loop->>Loop: unwrap + parse Message
  Loop->>DB: check_trade_index
  Loop->>Router: dispatch(Action)
  Router->>Mod: handler(...)
  par side-effects
    Mod->>DB: read/write
    Mod->>LND: hold/settle/cancel/pay
  end
```
