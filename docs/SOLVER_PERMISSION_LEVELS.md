# Solver Permission Levels

This document specifies solver permission levels for dispute resolution.

## Summary

Mostro supports two solver permission levels:

- `read`: solver can take a dispute, receive dispute context, and communicate with users
- `read-write`: solver can do everything above and can also execute `admin-settle` and `admin-cancel`

This split is intended to support automated dispute assistants, including AI-based agents, without giving them authority to move funds.

## Goals

- allow non-human dispute assistants to participate safely
- enforce authorization at the daemon level, not in prompts or UI
- preserve backward compatibility for existing solver registration flows

## Data Model

The `users.category` field is used to represent solver permissions:

- `0`: regular user / no solver permissions
- `1`: solver with `read` permission only
- `2`: solver with `read-write` permission

The legacy `is_solver` flag still indicates whether the user is a solver at all.

## Authorization Rules

### `admin-take-dispute`
Allowed for:
- Mostro daemon admin key while dispute status is `initiated` or `in-progress`
- solvers with `is_solver = true` while dispute status is `initiated`

Both `read` and `read-write` solvers may take a dispute.

### `admin-settle`
Allowed only when:
- the caller is the solver assigned to the dispute
- and the assigned solver has `category = 2`

If the caller is assigned but only has `read` permission, Mostro returns:
- `CantDoReason::NotAuthorized`

### `admin-cancel`
Allowed only when:
- the caller is the solver assigned to the dispute
- and the assigned solver has `category = 2`

If the caller is assigned but only has `read` permission, Mostro returns:
- `CantDoReason::NotAuthorized`

## AdminAddSolver payload

`admin-add-solver` continues using `Payload::TextMessage`, but now supports an optional permission suffix.

Formats:

- `npub1...` → defaults to `read-write`
- `npub1...:read` → registers solver as read-only
- `npub1...:read-write` → registers solver as read-write
- `npub1...:write` → alias for read-write

Invalid suffixes must be rejected with `CantDoReason::InvalidParameters`.

## RPC impact

The current RPC `AddSolverRequest` still only exposes `solver_pubkey`.

That means RPC registration remains backward compatible and defaults to `read-write` until the protobuf/API is extended.

## Dependency

This feature requires `mostro-core >= 0.8.4` because it uses `CantDoReason::NotAuthorized`.

## Security rationale

The key security property is that read-only solvers can never execute dispute-closing actions, even if:

- a UI exposes the wrong button
- an operator misconfigures an agent prompt
- a remote tool attempts to call `admin-settle` or `admin-cancel` directly

The daemon enforces the permission boundary.
