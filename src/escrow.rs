//! Escrow backend abstraction.
//!
//! Mostro can hold trade funds in escrow in more than one way. Today the only
//! production backend is **Lightning** (hold invoices on an LND node). A second,
//! opt-in **Cashu** 2-of-3 multisig backend is being added incrementally (see
//! `docs/CASHU_ESCROW_ARCHITECTURE.md`).
//!
//! [`EscrowBackend`] is the seam between the action handlers and whichever escrow
//! mechanism a node is configured with. Handlers receive `&mut dyn EscrowBackend`
//! instead of a concrete connector, so the same `release` / `cancel` / `admin_*`
//! code paths drive either backend. The Lightning implementation lives on
//! [`LndConnector`] and is a thin pass-through to its inherent methods —
//! behaviour-preserving versus calling those methods directly. The
//! [`CashuBackend`] methods are intentionally stubbed (`unimplemented!()`); the
//! Cashu feature tracks fill them in (Track A: lock, Track B: release, Track C:
//! cooperative cancel, Track D: dispute resolution).
//!
//! This trait keeps today's hold-invoice primitives as method names. It is the
//! narrow set the threaded `ln_client` actually exercises in the escrow flow;
//! payout (`send_payment`) and invoice subscription are not part of the escrow
//! seam and remain on the concrete [`LndConnector`].

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

/// The escrow operations the order-action handlers depend on.
///
/// Implemented by [`LndConnector`] (Lightning) and [`CashuBackend`] (Cashu).
/// `Send` is a supertrait so `dyn EscrowBackend` is `Send` and can be held
/// across `.await` points inside the handlers.
#[async_trait]
pub trait EscrowBackend: Send {
    /// Lock the trade amount in escrow.
    ///
    /// Lightning: create a hold invoice for `amount` sats with `description`,
    /// returning the LND response plus `(preimage, hash)`.
    async fn create_hold_invoice(
        &mut self,
        description: &str,
        amount: i64,
    ) -> Result<HoldInvoice, MostroError>;

    /// Release escrowed funds to the buyer.
    ///
    /// Lightning: settle the seller's hold invoice with `preimage`.
    async fn settle_hold_invoice(&mut self, preimage: &str) -> Result<(), MostroError>;

    /// Return escrowed funds to the seller / void the escrow.
    ///
    /// Lightning: cancel the hold invoice identified by `hash`.
    async fn cancel_hold_invoice(&mut self, hash: &str) -> Result<(), MostroError>;
}

/// Lightning escrow backend: a thin pass-through to [`LndConnector`]'s inherent
/// hold-invoice methods. The settle/cancel responses LND returns are discarded
/// here because the escrow flow only cares whether the call succeeded.
#[async_trait]
impl EscrowBackend for LndConnector {
    async fn create_hold_invoice(
        &mut self,
        description: &str,
        amount: i64,
    ) -> Result<HoldInvoice, MostroError> {
        let (resp, preimage, hash) =
            LndConnector::create_hold_invoice(self, description, amount).await?;
        Ok(HoldInvoice {
            payment_request: resp.payment_request,
            preimage,
            hash,
        })
    }

    async fn settle_hold_invoice(&mut self, preimage: &str) -> Result<(), MostroError> {
        LndConnector::settle_hold_invoice(self, preimage).await?;
        Ok(())
    }

    async fn cancel_hold_invoice(&mut self, hash: &str) -> Result<(), MostroError> {
        LndConnector::cancel_hold_invoice(self, hash).await?;
        Ok(())
    }
}

/// Cashu 2-of-3 multisig escrow backend.
///
/// A placeholder for the opt-in Cashu mode. In Cashu mode Mostro is only a
/// coordinator and never takes custody, so these hold-invoice primitives do not
/// map onto Cashu directly — the feature tracks replace the escrow paths that
/// call them. Until then every method returns a typed error (never `panic!`)
/// and the backend is never instantiated (the daemon defaults to Lightning).
/// Returning `Err` rather than `unimplemented!()` keeps an accidental future
/// instantiation from crashing the daemon in the middle of a trade.
#[derive(Debug, Default, Clone, Copy)]
pub struct CashuBackend;

impl CashuBackend {
    /// Error returned by every not-yet-implemented Cashu escrow primitive.
    fn not_implemented(primitive: &str) -> MostroError {
        MostroError::MostroInternalErr(ServiceError::UnexpectedError(format!(
            "Cashu escrow {primitive} is not implemented yet"
        )))
    }
}

#[async_trait]
impl EscrowBackend for CashuBackend {
    async fn create_hold_invoice(
        &mut self,
        _description: &str,
        _amount: i64,
    ) -> Result<HoldInvoice, MostroError> {
        Err(Self::not_implemented("lock"))
    }

    async fn settle_hold_invoice(&mut self, _preimage: &str) -> Result<(), MostroError> {
        Err(Self::not_implemented("release"))
    }

    async fn cancel_hold_invoice(&mut self, _hash: &str) -> Result<(), MostroError> {
        Err(Self::not_implemented("cancel"))
    }
}
