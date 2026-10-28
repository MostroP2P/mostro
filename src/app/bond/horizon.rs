//! CLTV horizon of a maker bond (#1084).
//!
//! A maker bond locks before its order is published and stays locked while
//! the order rests in the book, then through the whole trade that follows.
//! Its hold invoice is created with `bond_cltv_delta`, so the time the order
//! may rest in the book is what is left once a full trade
//! (`hold_invoice_cltv_delta`) and [`BOND_TRADE_MARGIN_BLOCKS`] are set
//! aside. These helpers turn that budget into a wall-clock deadline (the
//! clamp applied to `expires_at` when the bond locks, counted from the bond's
//! HTLC) and into a chain-height check (the guardian that covers blocks found
//! faster than the 10-minute average the clamp assumes).

use mostro_core::prelude::*;

use crate::config::constants::BOND_TRADE_MARGIN_BLOCKS;
use crate::escrow::EscrowBackend;
use crate::util::SECS_PER_BLOCK;

/// Blocks an order may rest in the book on its maker bond:
/// `bond_cltv_delta - hold_invoice_cltv_delta - BOND_TRADE_MARGIN_BLOCKS`.
/// Startup validation keeps it positive; it saturates at 0 for settings
/// built without that validation.
pub fn maker_bond_book_blocks(bond_cltv_delta: u32, hold_invoice_cltv_delta: u32) -> u32 {
    bond_cltv_delta
        .saturating_sub(hold_invoice_cltv_delta)
        .saturating_sub(BOND_TRADE_MARGIN_BLOCKS)
}

/// [`maker_bond_book_blocks`] in seconds, at the nominal 10-minute block.
pub fn book_window_secs(book_blocks: u32) -> i64 {
    i64::from(book_blocks) * SECS_PER_BLOCK
}

/// The `expires_at` an order gets when its maker bond locks at `now`: the
/// one the maker asked for, unless it outlives the book window. A
/// non-positive `expires_at` (a legacy row without one; new orders always
/// get one) takes the window's end, the deadline the bond allows.
pub fn clamp_book_expiry(expires_at: i64, now: i64, book_blocks: u32) -> i64 {
    let window_end = now.saturating_add(book_window_secs(book_blocks));
    if expires_at <= 0 {
        return window_end;
    }
    expires_at.min(window_end)
}

/// True when the order resting on a maker bond must leave the book now: the
/// bond's accepted HTLC can no longer cover a full trade plus
/// [`BOND_TRADE_MARGIN_BLOCKS`] from `chain_tip`, or no accepted HTLC backs
/// the bond anymore (`accepted_expiry` is `None`: LND canceled or settled
/// it).
pub fn maker_bond_horizon_due(
    chain_tip: u32,
    accepted_expiry: Option<u32>,
    hold_invoice_cltv_delta: u32,
) -> bool {
    let Some(expiry) = accepted_expiry else {
        return true;
    };
    chain_tip
        .saturating_add(hold_invoice_cltv_delta)
        .saturating_add(BOND_TRADE_MARGIN_BLOCKS)
        >= expiry
}

/// Blocks a maker bond whose accepted HTLC expires at `accepted_expiry`
/// still leaves its order in the book, seen from `chain_tip`: what is left
/// before [`maker_bond_horizon_due`], which holds exactly when this is 0.
pub fn maker_bond_book_blocks_left(
    chain_tip: u32,
    accepted_expiry: Option<u32>,
    hold_invoice_cltv_delta: u32,
) -> u32 {
    let Some(expiry) = accepted_expiry else {
        return 0;
    };
    expiry.saturating_sub(
        chain_tip
            .saturating_add(hold_invoice_cltv_delta)
            .saturating_add(BOND_TRADE_MARGIN_BLOCKS),
    )
}

/// Blocks the order of a maker bond that has just locked may rest in the
/// book: `configured_blocks` ([`maker_bond_book_blocks`]), cut to what the
/// bond's accepted HTLC still leaves ([`maker_bond_book_blocks_left`]).
///
/// The lock can be processed long after the HTLC arrived: the daemon was
/// down when the maker paid, and the restart resubscriber reports the
/// payment late. Counting the window from the HTLC keeps that delay out of
/// the book time. Errors when LND cannot be asked; the caller then counts
/// the configured window from now, and the guardian still checks the HTLC.
pub async fn maker_bond_book_blocks_at_lock(
    escrow: &mut dyn EscrowBackend,
    bond_hash: &str,
    configured_blocks: u32,
    hold_invoice_cltv_delta: u32,
) -> Result<u32, MostroError> {
    let chain_tip = escrow.chain_height().await?;
    let accepted_expiry = escrow.hold_invoice_expiry_height(bond_hash).await?;
    Ok(configured_blocks.min(maker_bond_book_blocks_left(
        chain_tip,
        accepted_expiry,
        hold_invoice_cltv_delta,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn book_blocks_is_what_the_bond_leaves_after_a_trade_and_the_margin() {
        // Shipped defaults: 432 - 144 - 144 = 144 blocks (~24 h).
        assert_eq!(maker_bond_book_blocks(432, 144), 144);
        assert_eq!(maker_bond_book_blocks(864, 144), 576);
        assert_eq!(maker_bond_book_blocks(289, 144), 1);
    }

    #[test]
    fn book_blocks_saturates_at_zero() {
        assert_eq!(maker_bond_book_blocks(288, 144), 0);
        assert_eq!(maker_bond_book_blocks(144, 144), 0);
        assert_eq!(maker_bond_book_blocks(0, u32::MAX), 0);
    }

    #[test]
    fn book_window_secs_counts_ten_minute_blocks() {
        assert_eq!(book_window_secs(144), 86_400);
        assert_eq!(book_window_secs(0), 0);
    }

    #[test]
    fn clamp_cuts_an_expiration_past_the_book_window() {
        let now = 1_700_000_000;
        // 15 days requested, 144 blocks of book time.
        let requested = now + 15 * 86_400;
        assert_eq!(clamp_book_expiry(requested, now, 144), now + 86_400);
    }

    #[test]
    fn clamp_keeps_an_expiration_inside_the_book_window() {
        let now = 1_700_000_000;
        assert_eq!(clamp_book_expiry(now + 3_600, now, 144), now + 3_600);
        // Exactly at the window's end is already inside it.
        assert_eq!(clamp_book_expiry(now + 86_400, now, 144), now + 86_400);
    }

    #[test]
    fn clamp_bounds_a_legacy_expiration() {
        let now = 1_700_000_000;
        assert_eq!(clamp_book_expiry(0, now, 144), now + 86_400);
        assert_eq!(clamp_book_expiry(-1, now, 144), now + 86_400);
    }

    #[test]
    fn horizon_is_due_once_the_bond_cannot_cover_a_trade_and_the_margin() {
        // hold 144 + margin 144: due when 288 blocks or fewer are left.
        let tip = 900_000;
        assert!(!maker_bond_horizon_due(tip, Some(tip + 289), 144));
        assert!(maker_bond_horizon_due(tip, Some(tip + 288), 144));
        assert!(maker_bond_horizon_due(tip, Some(tip + 10), 144));
        // Already past its expiry.
        assert!(maker_bond_horizon_due(tip, Some(tip - 5), 144));
    }

    #[test]
    fn horizon_is_due_when_no_accepted_htlc_backs_the_bond() {
        assert!(maker_bond_horizon_due(900_000, None, 144));
    }

    #[test]
    fn horizon_check_saturates() {
        // A huge tip must not wrap around and read as "far from expiry".
        assert!(maker_bond_horizon_due(u32::MAX, Some(u32::MAX), 144));
        assert!(maker_bond_horizon_due(u32::MAX - 10, Some(u32::MAX), 144));
    }

    #[test]
    fn book_blocks_left_is_what_the_htlc_leaves_before_the_horizon_is_due() {
        let tip = 900_000;
        // A fresh 432-block HTLC leaves the full default window.
        assert_eq!(maker_bond_book_blocks_left(tip, Some(tip + 432), 144), 144);
        assert_eq!(maker_bond_book_blocks_left(tip, Some(tip + 289), 144), 1);
        assert_eq!(maker_bond_book_blocks_left(tip, Some(tip + 288), 144), 0);
        assert_eq!(maker_bond_book_blocks_left(tip, Some(tip - 5), 144), 0);
        assert_eq!(maker_bond_book_blocks_left(tip, None, 144), 0);
        assert_eq!(
            maker_bond_book_blocks_left(u32::MAX, Some(u32::MAX), 144),
            0
        );
    }

    #[test]
    fn book_blocks_left_runs_out_exactly_when_the_horizon_is_due() {
        let tip = 900_000;
        for expiry in (tip + 280..tip + 300).map(Some).chain([None]) {
            assert_eq!(
                maker_bond_book_blocks_left(tip, expiry, 144) == 0,
                maker_bond_horizon_due(tip, expiry, 144),
                "expiry {expiry:?}"
            );
        }
    }

    /// Chain view handed to [`maker_bond_book_blocks_at_lock`]: `(tip,
    /// accepted HTLC expiry)`, or `None` when LND cannot be asked.
    struct ChainView(Option<(u32, Option<u32>)>);

    fn no_chain_view() -> MostroError {
        MostroInternalErr(ServiceError::LnNodeError("no chain view".to_string()))
    }

    #[async_trait::async_trait]
    impl EscrowBackend for ChainView {
        async fn create_hold_invoice(
            &mut self,
            _description: &str,
            _amount: i64,
        ) -> Result<(String, Vec<u8>, Vec<u8>), MostroError> {
            unreachable!("the book window never creates invoices")
        }

        async fn settle_hold_invoice(&mut self, _preimage: &str) -> Result<(), MostroError> {
            unreachable!("the book window never settles")
        }

        async fn cancel_hold_invoice(&mut self, _hash: &str) -> Result<(), MostroError> {
            unreachable!("the book window never cancels")
        }

        async fn chain_height(&mut self) -> Result<u32, MostroError> {
            self.0.map(|(tip, _)| tip).ok_or_else(no_chain_view)
        }

        async fn hold_invoice_expiry_height(
            &mut self,
            _hash: &str,
        ) -> Result<Option<u32>, MostroError> {
            self.0.map(|(_, expiry)| expiry).ok_or_else(no_chain_view)
        }
    }

    #[tokio::test]
    async fn book_blocks_at_lock_counts_from_the_htlc_not_from_the_lock() {
        // The HTLC arrived 60 blocks before its lock was processed (the
        // daemon was down when the maker paid): 432 - 60 - 288 = 84 left.
        let tip = 900_000;
        let mut chain = ChainView(Some((tip, Some(tip + 372))));

        let blocks = maker_bond_book_blocks_at_lock(&mut chain, "hash", 144, 144).await;

        assert_eq!(blocks.unwrap(), 84);
    }

    #[tokio::test]
    async fn book_blocks_at_lock_never_exceeds_the_configured_window() {
        // A payer that padded the final CLTV gives the HTLC more room than
        // `bond_cltv_delta`; the book window stays the configured one.
        let tip = 900_000;
        let mut chain = ChainView(Some((tip, Some(tip + 500))));

        let blocks = maker_bond_book_blocks_at_lock(&mut chain, "hash", 144, 144).await;

        assert_eq!(blocks.unwrap(), 144);
    }

    #[tokio::test]
    async fn book_blocks_at_lock_is_zero_without_an_accepted_htlc() {
        let mut chain = ChainView(Some((900_000, None)));

        let blocks = maker_bond_book_blocks_at_lock(&mut chain, "hash", 144, 144).await;

        assert_eq!(blocks.unwrap(), 0);
    }

    #[tokio::test]
    async fn book_blocks_at_lock_errors_without_a_chain_view() {
        let mut chain = ChainView(None);

        let blocks = maker_bond_book_blocks_at_lock(&mut chain, "hash", 144, 144).await;

        assert!(blocks.is_err());
    }
}
