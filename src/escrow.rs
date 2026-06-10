//! Escrow backend abstraction.
//!
//! Mostro can hold trade funds in escrow in more than one way. Today the only
//! production backend is **Lightning** (hold invoices on an LND node). A second,
//! opt-in **Cashu** 2-of-3 multisig backend is being added incrementally (see
//! `docs/CASHU_ESCROW_ARCHITECTURE.md`).
//!
//! [`EscrowBackend`] is the seam between the action handlers and whichever escrow
//! mechanism a node is configured with. Handlers reach the backend through
//! `AppContext::escrow()` (an `Arc<dyn EscrowBackend>`), so the same
//! `lock` / `release` / `cooperative_cancel` / `dispute_*` code paths drive
//! either backend.
//!
//! The trait is *semantic*: its methods name escrow operations
//! (`lock`/`release`/`cooperative_cancel`/`dispute_settle`/`dispute_cancel`)
//! rather than the underlying Lightning primitives. [`LightningBackend`] maps
//! those operations onto today's hold-invoice calls — behaviour-preserving
//! versus calling them directly. The Cashu backend
//! ([`crate::cashu::CashuBackend`]) fills the same trait in incrementally; the
//! feature tracks implement one method at a time (Track A: lock, Track B:
//! release, Track C: cooperative cancel, Track D: dispute resolution).
//!
//! Methods take `&self` (interior mutability) and the trait is `Send + Sync`, so
//! a single `Arc<dyn EscrowBackend>` can live in `AppContext` and be shared
//! across handlers and `.await` points. Payout (`send_payment`) and invoice
//! subscription are not part of the escrow seam and remain on the concrete
//! [`LndConnector`].

use crate::lightning::LndConnector;
use async_trait::async_trait;
use mostro_core::prelude::*;

/// Backend-agnostic result of opening an escrow "lock".
///
/// For Lightning this carries the hold invoice: `payment_request` is the BOLT11
/// string the counterparty pays, and `(preimage, hash)` identify and later
/// settle it. Keeping this struct (rather than LND's `AddHoldInvoiceResp`) out
/// of the trait keeps `fedimint-tonic-lnd` out of the escrow contract, so a
/// non-Lightning backend can implement `EscrowBackend` without depending on it.
#[derive(Debug, Clone)]
pub struct HoldInvoice {
    /// BOLT11 invoice the counterparty pays (Lightning).
    pub payment_request: String,
    /// Preimage that releases the locked funds.
    pub preimage: Vec<u8>,
    /// Payment hash identifying the escrow.
    pub hash: Vec<u8>,
}

/// The escrow operations the order-action handlers call instead of touching
/// LND (or, later, cdk) directly.
///
/// `&self` + `Send + Sync` so a single `Arc<dyn EscrowBackend>` can live in
/// `AppContext` and be shared. Implemented by [`LightningBackend`] (Lightning)
/// and [`crate::cashu::CashuBackend`] (Cashu).
#[async_trait]
pub trait EscrowBackend: Send + Sync {
    /// Open the escrow lock. Lightning: create a hold invoice for `amount`
    /// sats described by `description`. The `order` is provided for backends
    /// (Cashu) whose lock is derived from order state.
    async fn lock(
        &self,
        order: &Order,
        description: &str,
        amount: i64,
    ) -> Result<HoldInvoice, MostroError>;

    /// Release escrowed funds to the buyer. Lightning: settle the hold invoice.
    async fn release(&self, order: &Order) -> Result<(), MostroError>;

    /// Mutual / non-dispute cancel — return funds to seller. Lightning: cancel
    /// the hold invoice.
    async fn cooperative_cancel(&self, order: &Order) -> Result<(), MostroError>;

    /// Admin resolves a dispute in the buyer's favor. Lightning: settle the
    /// hold invoice.
    async fn dispute_settle(&self, order: &Order) -> Result<(), MostroError>;

    /// Admin resolves a dispute in the seller's favor. Lightning: cancel the
    /// hold invoice.
    async fn dispute_cancel(&self, order: &Order) -> Result<(), MostroError>;
}

/// Lightning escrow backend: maps the semantic escrow operations onto
/// [`LndConnector`]'s inherent hold-invoice methods.
///
/// Methods are `&self`, but the connector's methods are `&mut self`. The
/// connector's tonic client is cheap to clone, so we hold an owned
/// [`LndConnector`] and clone it per call to get the `&mut` each primitive
/// needs. This keeps the backend shareable behind an `Arc<dyn EscrowBackend>`.
#[derive(Clone)]
pub struct LightningBackend {
    conn: LndConnector,
}

impl LightningBackend {
    /// Wrap an existing [`LndConnector`] as an escrow backend.
    pub fn new(conn: LndConnector) -> Self {
        Self { conn }
    }

    /// Borrow a fresh, cloned [`LndConnector`] for LN-only subsystems.
    ///
    /// The anti-abuse bond and dev-fee payout are LN-only and not part of the
    /// escrow seam; the handlers that drive them obtain a concrete connector
    /// from here in Lightning mode.
    pub fn connector(&self) -> LndConnector {
        self.conn.clone()
    }
}

#[async_trait]
impl EscrowBackend for LightningBackend {
    async fn lock(
        &self,
        _order: &Order,
        description: &str,
        amount: i64,
    ) -> Result<HoldInvoice, MostroError> {
        let mut conn = self.conn.clone();
        let (resp, preimage, hash) = conn.create_hold_invoice(description, amount).await?;
        Ok(HoldInvoice {
            payment_request: resp.payment_request,
            preimage,
            hash,
        })
    }

    async fn release(&self, order: &Order) -> Result<(), MostroError> {
        settle_for_order(&self.conn, order).await
    }

    async fn cooperative_cancel(&self, order: &Order) -> Result<(), MostroError> {
        cancel_for_order(&self.conn, order).await
    }

    async fn dispute_settle(&self, order: &Order) -> Result<(), MostroError> {
        settle_for_order(&self.conn, order).await
    }

    async fn dispute_cancel(&self, order: &Order) -> Result<(), MostroError> {
        cancel_for_order(&self.conn, order).await
    }
}

/// Settle the order's hold invoice using its stored preimage. Mirrors the
/// previous `settle_seller_hold_invoice` behaviour: a missing preimage is an
/// `InvalidInvoice` error.
async fn settle_for_order(conn: &LndConnector, order: &Order) -> Result<(), MostroError> {
    let preimage = order
        .preimage
        .as_ref()
        .ok_or(MostroCantDo(CantDoReason::InvalidInvoice))?;
    let mut conn = conn.clone();
    conn.settle_hold_invoice(preimage).await?;
    Ok(())
}

/// Cancel the order's hold invoice using its stored hash. Mirrors the previous
/// cancel-path behaviour: callers guard on `order.hash` being present, so a
/// missing hash here is treated as a no-op.
async fn cancel_for_order(conn: &LndConnector, order: &Order) -> Result<(), MostroError> {
    if let Some(hash) = order.hash.as_ref() {
        let mut conn = conn.clone();
        conn.cancel_hold_invoice(hash).await?;
    }
    Ok(())
}

#[cfg(test)]
pub mod test_utils {
    //! Test doubles for [`EscrowBackend`].

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Recording mock for [`EscrowBackend`].
    ///
    /// Returns success for every operation (a dummy [`HoldInvoice`] for `lock`)
    /// and records which methods were called so tests can assert "settle was
    /// called once" / "cancel was called", replacing the previous
    /// `CancelLightning` / `SettleLightning` stubs.
    #[derive(Debug, Default, Clone)]
    pub struct MockEscrowBackend {
        lock_calls: Arc<AtomicUsize>,
        release_calls: Arc<AtomicUsize>,
        cooperative_cancel_calls: Arc<AtomicUsize>,
        dispute_settle_calls: Arc<AtomicUsize>,
        dispute_cancel_calls: Arc<AtomicUsize>,
        /// Ordered log of method names invoked.
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl MockEscrowBackend {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn lock_count(&self) -> usize {
            self.lock_calls.load(Ordering::SeqCst)
        }

        pub fn release_count(&self) -> usize {
            self.release_calls.load(Ordering::SeqCst)
        }

        pub fn cooperative_cancel_count(&self) -> usize {
            self.cooperative_cancel_calls.load(Ordering::SeqCst)
        }

        pub fn dispute_settle_count(&self) -> usize {
            self.dispute_settle_calls.load(Ordering::SeqCst)
        }

        pub fn dispute_cancel_count(&self) -> usize {
            self.dispute_cancel_calls.load(Ordering::SeqCst)
        }

        /// Snapshot of the ordered method-call log.
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, name: &str) {
            self.calls.lock().unwrap().push(name.to_string());
        }
    }

    #[async_trait]
    impl EscrowBackend for MockEscrowBackend {
        async fn lock(
            &self,
            _order: &Order,
            _description: &str,
            _amount: i64,
        ) -> Result<HoldInvoice, MostroError> {
            self.lock_calls.fetch_add(1, Ordering::SeqCst);
            self.record("lock");
            Ok(HoldInvoice {
                payment_request: "lnbc-mock".to_string(),
                preimage: vec![0u8; 32],
                hash: vec![0u8; 32],
            })
        }

        async fn release(&self, _order: &Order) -> Result<(), MostroError> {
            self.release_calls.fetch_add(1, Ordering::SeqCst);
            self.record("release");
            Ok(())
        }

        async fn cooperative_cancel(&self, _order: &Order) -> Result<(), MostroError> {
            self.cooperative_cancel_calls.fetch_add(1, Ordering::SeqCst);
            self.record("cooperative_cancel");
            Ok(())
        }

        async fn dispute_settle(&self, _order: &Order) -> Result<(), MostroError> {
            self.dispute_settle_calls.fetch_add(1, Ordering::SeqCst);
            self.record("dispute_settle");
            Ok(())
        }

        async fn dispute_cancel(&self, _order: &Order) -> Result<(), MostroError> {
            self.dispute_cancel_calls.fetch_add(1, Ordering::SeqCst);
            self.record("dispute_cancel");
            Ok(())
        }
    }
}
