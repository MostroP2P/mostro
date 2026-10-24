//! CLTV horizon of a maker bond (#1084).
//!
//! A maker bond locks before its order is published and stays locked while
//! the order rests in the book, then through the whole trade that follows.
//! Its hold invoice is created with `bond_cltv_delta`, so the time the order
//! may rest in the book is what is left once a full trade
//! (`hold_invoice_cltv_delta`) and [`BOND_TRADE_MARGIN_BLOCKS`] are set
//! aside. These pure helpers turn that budget into a wall-clock deadline (the
//! clamp applied to `expires_at` when the bond locks) and into a chain-height
//! check (the guardian that covers blocks found faster than the 10-minute
//! average the clamp assumes).

use crate::config::constants::BOND_TRADE_MARGIN_BLOCKS;
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
}
