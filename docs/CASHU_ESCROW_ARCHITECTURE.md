# Cashu 2-of-3 Multisig Escrow Architecture

This document describes the alternative escrow mechanism utilizing Cashu (NUT-11 P2PK) rather than Lightning Network hold invoices. In this model, Mostro acts strictly as a coordinator and arbitrator, and never takes custody of funds. 

## Module Map (Proposed)

```mermaid
flowchart LR
  subgraph Runtime[src/]
    MAIN[src/main.rs]
    APP[src/app.rs]
    A_APP[src/app/*]
    CASHU[src/cashu/mod.rs]
    RPC[src/rpc/server.rs]
    DB[src/db.rs]
  end

  MAIN --> APP
  APP --> A_APP
  APP --> DB
  APP --> CASHU
  RPC --> DB
  RPC --> CASHU
```

- Entry: `src/main.rs` initializes standard subsystems but bypasses `fedimint-tonic-lnd` initialization if operating in pure-Cashu mode.
- Cashu: `src/cashu/mod.rs` interfaces with the `cdk` crate to verify token conditions (NUT-10/NUT-11) and communicate with the Mint's `/v1/checkstate` endpoint.

## Core Flow: 2-of-3 Multisig

Instead of routing HTLCs through Mostro's node, the seller locks the funds in a Cashu token governed by a `2-of-3` signature requirement:
1. $P_B$ (Buyer Pubkey)
2. $P_S$ (Seller Pubkey)
3. $P_M$ (Mostro/Arbitrator Pubkey)

```mermaid
sequenceDiagram
    participant S as Seller (Maker)
    participant M as Mostro
    participant B as Buyer (Taker)
    participant Mint as Cashu Mint

    Note over S,B: 1. ORDER MATCHING
    B->>M: Take Sell (Take Order)
    M->>S: Notify: Order Taken, Provide P_B and P_M

    Note over S,Mint: 2. ESCROW SETUP (No Mostro Custody)
    S->>Mint: Swap unencumbered ecash for 2-of-3 Locked ecash
    S->>M: Submit locked tokens & condition details
    M->>Mint: Checkstate (Are tokens unspent?)
    M->>B: Notify: Escrow Locked. Send Fiat!

    Note over B,S: 3. FIAT TRANSFER
    B->>M: Fiat Sent
    M->>S: Notify: Check your bank

    Note over S,B: 4. RELEASE (Happy Path)
    S->>M: Sign inputs (Sig 1)
    M->>B: Forward Seller's Signature
    B->>Mint: Submit SwapRequest (Sig 1 + Sig 2)
    Mint-->>B: Unencumbered ecash issued to Buyer
```

## Action Changes & Handlers

The introduction of Cashu Escrow modifies the responsibility of core action handlers.

| Action | Proposed Handler Mod | Responsibility |
| --- | --- | --- |
| `add-invoice` | `src/app/add_invoice.rs` | Instead of creating a hold invoice, validates the submitted Cashu token using `cdk`, verifies the 2-of-3 spending condition, and calls the Mint API to ensure funds exist. |
| `release` | `src/app/release.rs` | Instead of resolving an HTLC, Mostro simply accepts the Seller's cryptographic signature, appends it to the trade state, and relays the signature to the Buyer. |
| `cancel` | `src/app/cancel.rs` | If a trade is canceled cooperatively, the Buyer provides their signature to the Seller so the Seller can reclaim the locked ecash. |
| `admin-settle` | `src/app/admin_settle.rs` | (Dispute Resolution) Mostro generates its signature ($P_M$) and hands it to the Buyer, allowing the Buyer to construct a valid 2-of-3 SwapRequest. |
| `admin-cancel` | `src/app/admin_cancel.rs` | (Dispute Resolution) Mostro generates its signature ($P_M$) and hands it to the Seller, allowing the Seller to reclaim their funds. |

## CDK Implementation Details

### Generating Spending Conditions
Sellers construct the 2-of-3 spending condition using `cdk::nuts::nut10`. We recommend the `SIG_INPUTS` flag. This allows the seller to sign the authorization once and pass it to the buyer, allowing the buyer to specify their own target outputs independently.

```rust
use cdk::nuts::nut10::{Conditions, SpendingConditions, SigFlag};
use cdk::nuts::PublicKey;

// 1. Gather pubkeys
let p_s: PublicKey = /* Seller */;
let p_b: PublicKey = /* Buyer */;
let p_m: PublicKey = /* Mostro */;

// 2. Define 2-of-3 constraints
let conditions = Conditions::new(
    None,                           
    Some(vec![p_b, p_m]),           // Secondary keys
    None,                           
    Some(2),                        // Requires 2 signatures
    None,                           
    Some(SigFlag::SigInputs),       // SigInputs for flexible output assignment
).unwrap();

// 3. Generate Secret for blinding
let secret = SpendingConditions::new_p2pk(p_s, Some(conditions));
```

### Signature Flags: `SIG_INPUTS` vs `SIG_ALL`
*   **`SIG_INPUTS`:** The easiest UX. The Seller only signs the intent to release. The Buyer receives the signature via Nostr DM, crafts their own unblinded outputs, signs the request, and asks the Mint to swap.
*   **`SIG_ALL`:** The safest UX against malicious Mints. The Buyer must pre-construct their outputs, send the hash to the Seller, and the Seller signs the entire bundle. 
*   **Decision:** Mostro relies on `SIG_INPUTS` as the baseline. Because both parties must mutually agree on the Mint provider prior to the trade, we assume the Mint will not maliciously front-run transaction outputs. 

## Advantages over Lightning Hold Invoices

1. **Non-Custodial:** Mostro drops all legal and technical burdens of custody. A compromised Mostro server only leaks 1 of 3 keys, meaning attacker cannot steal active escrows.
2. **Offline Resilience:** If Mostro's daemon crashes or vanishes permanently, the Buyer and Seller can still cooperate out-of-band to settle the trade (Seller + Buyer = 2 keys).
3. **No Routing Failures:** Bypasses Lightning Network topology, channel liquidity constraints, and unpredictable routing fees.
4. **Zero Capital Lockup:** Mostro does not require inbound/outbound channel liquidity to facilitate trades.