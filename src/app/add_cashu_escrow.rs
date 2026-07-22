//! Cashu escrow lock handler — Track A **TA-1** integration point.
//!
//! CF-5 ships this as a **stub**: `dispatch_cashu` already routes
//! `Action::AddCashuEscrow` here, but until Track A lands the handler simply
//! rejects the action with `CantDo(InvalidAction)` — the same "not implemented
//! yet" contract every other trade action gets in Cashu foundation mode
//! (see `docs/cashu/01-fundamentals.md` §6 and `docs/cashu/02-track-a-lock.md`).
//!
//! Keeping the routing in `dispatch_cashu` frozen and the body in its own file
//! means Track A fills in the validation + atomic-lock algorithm **here**,
//! touching no shared file (the G-1 fix in `docs/cashu/02-track-a-lock.md` §11).

use crate::app::context::AppContext;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;

/// Handle a seller's `AddCashuEscrow` submission.
///
/// **Foundation stub (CF-5):** returns `CantDo(InvalidAction)` unconditionally.
/// Track A (TA-1) replaces this body with the full lock algorithm — validate
/// the 2-of-3 token against the mint and the order's trade keys, atomically
/// compare-and-set `WaitingPayment → Active`, publish the order event, and
/// notify the buyer to send fiat.
pub async fn add_cashu_escrow_action(
    _ctx: &AppContext,
    _msg: Message,
    _event: &UnwrappedMessage,
    _my_keys: &Keys,
) -> Result<(), MostroError> {
    Err(MostroError::MostroCantDo(CantDoReason::InvalidAction))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use sqlx::SqlitePool;
    use std::sync::Arc;

    /// CF-5 stub contract: until Track A lands, the handler rejects every
    /// submission with `CantDo(InvalidAction)` — no panic, no state change.
    #[tokio::test]
    async fn stub_rejects_with_invalid_action() {
        let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
        let ctx = TestContextBuilder::new()
            .with_pool(pool)
            .with_settings(test_settings())
            .build();
        let my_keys = Keys::generate();
        let event = mostro_core::nip59::UnwrappedMessage {
            message: Message::new_order(None, None, None, Action::AddCashuEscrow, None),
            signature: None,
            sender: Keys::generate().public_key(),
            identity: Keys::generate().public_key(),
            created_at: nostr_sdk::Timestamp::now(),
        };
        let msg = Message::new_order(None, None, None, Action::AddCashuEscrow, None);

        let result = add_cashu_escrow_action(&ctx, msg, &event, &my_keys).await;
        assert_eq!(
            result,
            Err(MostroError::MostroCantDo(CantDoReason::InvalidAction))
        );
    }
}
