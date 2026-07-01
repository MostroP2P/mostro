//! Escrow backend seam — Cashu foundation **CF-0**
//! (see `docs/cashu/01-fundamentals.md` §6).
//!
//! A narrow, hold-invoice-shaped trait so handlers can be routed through
//! `&mut dyn EscrowBackend` instead of a concrete [`LndConnector`]. Per the
//! locked design decision (fundamentals §4.7) the trait deliberately keeps
//! the LN hold-invoice shape: Cashu behaviour is added later by branching
//! handlers on `Settings::escrow_mode()`, never by widening this trait into
//! a high-level `lock`/`release`/`dispute` abstraction.

use async_trait::async_trait;
use mostro_core::prelude::*;

use crate::lightning::LndConnector;

/// The escrow seam. Implemented as a behaviour-preserving pass-through by
/// [`LndConnector`] and as an inert stub by [`CashuBackend`] until the
/// feature tracks land.
#[async_trait]
pub trait EscrowBackend: Send {
    /// Create a hold invoice for `amount` sats.
    ///
    /// Returns `(bolt11 payment_request, preimage bytes, hash bytes)`.
    async fn create_hold_invoice(
        &mut self,
        description: &str,
        amount: i64,
    ) -> Result<(String, Vec<u8>, Vec<u8>), MostroError>;

    /// Settle a held invoice with its preimage (hex).
    async fn settle_hold_invoice(&mut self, preimage: &str) -> Result<(), MostroError>;

    /// Cancel a held invoice by payment hash (hex).
    async fn cancel_hold_invoice(&mut self, hash: &str) -> Result<(), MostroError>;
}

#[async_trait]
impl EscrowBackend for LndConnector {
    async fn create_hold_invoice(
        &mut self,
        description: &str,
        amount: i64,
    ) -> Result<(String, Vec<u8>, Vec<u8>), MostroError> {
        // Fully-qualified call: the inherent method, not this trait method.
        let (resp, preimage, hash) =
            LndConnector::create_hold_invoice(self, description, amount).await?;
        Ok((resp.payment_request, preimage, hash))
    }

    async fn settle_hold_invoice(&mut self, preimage: &str) -> Result<(), MostroError> {
        LndConnector::settle_hold_invoice(self, preimage)
            .await
            .map(|_| ())
    }

    async fn cancel_hold_invoice(&mut self, hash: &str) -> Result<(), MostroError> {
        // The inherent method encodes the gRPC code as a stable
        // `code=<Code>` prefix in the error string (bond release relies on
        // it); delegating preserves that contract.
        LndConnector::cancel_hold_invoice(self, hash)
            .await
            .map(|_| ())
    }
}

/// Cashu escrow backend — CF-0 stub.
///
/// Every method returns a typed "not implemented" error (never panics).
/// Real Cashu behaviour arrives with the feature tracks
/// (`docs/cashu/02-track-a-lock.md` onward), which branch handlers on
/// `Settings::escrow_mode()` rather than implementing this trait.
#[derive(Debug, Default, Clone, Copy)]
pub struct CashuBackend;

impl CashuBackend {
    pub fn new() -> Self {
        Self
    }
}

fn cashu_not_implemented(op: &str) -> MostroError {
    MostroInternalErr(ServiceError::HoldInvoiceError(format!(
        "cashu escrow backend: {op} not implemented (foundation stub)"
    )))
}

#[async_trait]
impl EscrowBackend for CashuBackend {
    async fn create_hold_invoice(
        &mut self,
        _description: &str,
        _amount: i64,
    ) -> Result<(String, Vec<u8>, Vec<u8>), MostroError> {
        Err(cashu_not_implemented("create_hold_invoice"))
    }

    async fn settle_hold_invoice(&mut self, _preimage: &str) -> Result<(), MostroError> {
        Err(cashu_not_implemented("settle_hold_invoice"))
    }

    async fn cancel_hold_invoice(&mut self, _hash: &str) -> Result<(), MostroError> {
        Err(cashu_not_implemented("cancel_hold_invoice"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CF-0 merge gate: every stub method must return the typed error — a
    /// panic here would take the daemon down if a handler is ever misrouted
    /// before the tracks land.
    #[tokio::test]
    async fn cashu_backend_stub_returns_typed_error_without_panicking() {
        let mut backend = CashuBackend::new();

        let create = backend.create_hold_invoice("test", 1_000).await;
        let settle = backend.settle_hold_invoice("00").await;
        let cancel = backend.cancel_hold_invoice("00").await;

        for (op, result) in [
            ("create_hold_invoice", create.map(|_| ())),
            ("settle_hold_invoice", settle),
            ("cancel_hold_invoice", cancel),
        ] {
            match result {
                Err(MostroInternalErr(ServiceError::HoldInvoiceError(msg))) => {
                    assert!(
                        msg.contains("not implemented") && msg.contains(op),
                        "{op}: unexpected message {msg:?}"
                    );
                }
                other => panic!("{op}: expected typed not-implemented error, got {other:?}"),
            }
        }
    }
}
