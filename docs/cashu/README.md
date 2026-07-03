# Cashu Escrow — Specification Series

This directory holds the **working specification** for adding the optional
Cashu (NUT-11 P2PK 2-of-3 multisig) escrow mode to `mostrod`. It supersedes the
single high-level document [`../CASHU_ESCROW_ARCHITECTURE.md`](../CASHU_ESCROW_ARCHITECTURE.md),
which remains the canonical *architecture & motivation* reference. The documents
here are the *engineering plan*: how we land the feature on `main` in small,
safe, independently-mergeable pull requests.

## Why a re-plan

The first Cashu attempt was developed on `feat/cashu-mostro-core-0.12` against an
older `mostro-core`. `main` has since moved to `mostro-core 0.13.0`, which
**already ships the Cashu protocol surface** (actions, payloads, order fields,
errors). Re-landing the daemon work cleanly on top of `main` — rather than
force-merging the old branch — lets us keep `mostrod` (which is very stable)
untouched while the feature is off, and lets several developers work in parallel.

## The prime directive

> Every PR in this series must merge to `main` **without changing existing
> behaviour while Cashu is disabled**, and `main` must stay shippable after each
> merge. Cashu is **opt-in and defaults to off**. The Lightning hold-invoice flow
> is never modified in a behaviour-changing way during the foundation work.

## Documents

| # | Document | Scope | Status |
|---|----------|-------|--------|
| 00 | [`../CASHU_ESCROW_ARCHITECTURE.md`](../CASHU_ESCROW_ARCHITECTURE.md) | Architecture, motivation, crypto model, trust model | Reference |
| 01 | [`01-fundamentals.md`](./01-fundamentals.md) | **Foundation milestone** — config, mint client, DB helpers, test harness, boot wiring | **Draft (this PR)** |
| 02 | [`02-track-a-lock.md`](./02-track-a-lock.md) | Escrow lock / setup (`AddCashuEscrow`) | **Draft** |
| 03 | `03-track-b-release.md` | Release happy path | Planned |
| 04 | `04-track-c-coop-cancel.md` | Cooperative cancel | Planned |
| 05 | `05-track-d-dispute.md` | Dispute resolution (`P_M` signs) | Planned |

The **fundamentals** document (01) is the only one that touches shared,
conflict-prone files. Once it has merged, the feature tracks (02–05) edit
disjoint files and can proceed independently. Start there.

## How to read this

1. Read [`01-fundamentals.md`](./01-fundamentals.md) end to end.
2. Look at the **"Already on `main`"** table — do not re-implement those pieces.
3. Pick a fundamentals PR (`CF-0` … `CF-5`) from the **issues table**; the table
   says what it depends on and what it can run in parallel with.
4. Each PR has an explicit **Definition of Done** and **backwards-compatibility
   guarantee** — both are merge gates.
