//! Pure helpers for bond-amount computation.
//!
//! Kept separate from config and model layers so tests can exercise all
//! edge cases without spinning up a daemon. See issue #711 §Bond
//! Computation:
//!
//! ```text
//! bond_amount = max(amount_pct * order_amount_sats, base_amount_sats)
//! ```

use crate::config::AntiAbuseBondSettings;

/// Compute the bond size in satoshis for an order of `order_amount_sats`
/// using the operator's configured percentage / floor.
///
/// - `order_amount_sats` is clamped at 0: negative order amounts (which
///   should never reach here) yield the floor.
/// - The percentage result is rounded to the nearest sat.
/// - The floor is enforced so a tiny order never yields a trivial bond.
/// - Arithmetic is saturating: a pathological huge order can't overflow.
pub fn compute_bond_amount(order_amount_sats: i64, cfg: &AntiAbuseBondSettings) -> i64 {
    let base = cfg.base_amount_sats.max(0);
    if order_amount_sats <= 0 || cfg.amount_pct <= 0.0 {
        return base;
    }

    // f64 is sufficient here: we cap Lightning order amounts in sats well
    // below 2^53 at config time, so the multiplication is exact.
    let pct_raw = (order_amount_sats as f64) * cfg.amount_pct;
    let pct_rounded = pct_raw.round();

    // Saturate to i64 before comparing with the floor to guarantee no UB on
    // absurd inputs (e.g. `amount_pct = 1e18`).
    let pct_sats: i64 = if pct_rounded >= i64::MAX as f64 {
        i64::MAX
    } else if pct_rounded <= 0.0 {
        0
    } else {
        pct_rounded as i64
    };

    pct_sats.max(base)
}

/// Compute the node's share of a slashed bond, in sats.
///
/// `pct` is the operator's `slash_node_share_pct`, already validated by
/// the config deserializer to be in `[0.0, 1.0]`. The counterparty share
/// is always derived as `amount_sats - node_share_sats` by the caller,
/// so the two cannot drift and always sum exactly to `amount_sats` (the
/// spec's "no rounding leaks" invariant in §8.1).
///
/// Frozen at the moment the bond enters `PendingPayout`: Phase 2 writes
/// the result to the `node_share_sats` column in the same DB update
/// that flips the bond state, so a later config change or daemon restart
/// can never rebalance the split.
pub fn compute_node_share(amount_sats: i64, pct: f64) -> i64 {
    if amount_sats <= 0 {
        return 0;
    }
    // The config deserializer rejects values outside [0.0, 1.0], but be
    // defensive against a future call site that builds settings in code.
    let pct = pct.clamp(0.0, 1.0);
    let raw = (amount_sats as f64) * pct;
    // `floor` so the counterparty share, computed as `amount_sats - share`,
    // is never negative even at pct=1.0 and never strands a sat at pct<1.0.
    let floored = raw.floor();
    if floored <= 0.0 {
        0
    } else if floored >= amount_sats as f64 {
        amount_sats
    } else {
        floored as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(pct: f64, floor: i64) -> AntiAbuseBondSettings {
        AntiAbuseBondSettings {
            enabled: true,
            amount_pct: pct,
            base_amount_sats: floor,
            ..AntiAbuseBondSettings::default()
        }
    }

    #[test]
    fn floor_dominates_on_tiny_orders() {
        let cfg = cfg_with(0.01, 1_000);
        // 1% of 50_000 = 500 < floor
        assert_eq!(compute_bond_amount(50_000, &cfg), 1_000);
    }

    #[test]
    fn percentage_dominates_on_large_orders() {
        let cfg = cfg_with(0.01, 1_000);
        // 1% of 10_000_000 = 100_000 > floor
        assert_eq!(compute_bond_amount(10_000_000, &cfg), 100_000);
    }

    #[test]
    fn floor_and_percentage_equal_at_threshold() {
        // Example from the issue: 100k sats order, 1% pct, 1k floor → 1000.
        let cfg = cfg_with(0.01, 1_000);
        assert_eq!(compute_bond_amount(100_000, &cfg), 1_000);
    }

    #[test]
    fn zero_percentage_returns_floor() {
        let cfg = cfg_with(0.0, 500);
        assert_eq!(compute_bond_amount(1_000_000, &cfg), 500);
    }

    #[test]
    fn zero_order_returns_floor() {
        let cfg = cfg_with(0.01, 250);
        assert_eq!(compute_bond_amount(0, &cfg), 250);
    }

    #[test]
    fn negative_order_returns_floor() {
        let cfg = cfg_with(0.01, 250);
        assert_eq!(compute_bond_amount(-5_000, &cfg), 250);
    }

    #[test]
    fn negative_floor_clamped_to_zero() {
        let cfg = cfg_with(0.01, -10);
        // Percentage still applies; floor clamps to 0 so it cannot negate.
        assert_eq!(compute_bond_amount(10_000, &cfg), 100);
    }

    #[test]
    fn rounds_to_nearest_sat() {
        // 0.007 * 333 = 2.331 → 2
        let cfg = cfg_with(0.007, 0);
        assert_eq!(compute_bond_amount(333, &cfg), 2);
        // 0.007 * 500 = 3.5 → 4. `f64::round()` rounds half away from
        // zero, not half-to-even, so 3.5 → 4 deterministically.
        let cfg = cfg_with(0.007, 0);
        assert_eq!(compute_bond_amount(500, &cfg), 4);
    }

    #[test]
    fn saturates_on_absurd_percentage() {
        let cfg = cfg_with(1e18, 0);
        // No overflow; clamps to i64::MAX. Not a realistic config but
        // the guard prevents a panic.
        assert_eq!(compute_bond_amount(i64::MAX, &cfg), i64::MAX);
    }

    #[test]
    fn node_share_half_default() {
        // 10_000 sats at 50% → 5_000 node / 5_000 counterparty.
        assert_eq!(compute_node_share(10_000, 0.5), 5_000);
    }

    #[test]
    fn node_share_zero_pct_goes_to_counterparty() {
        // Legacy winner-takes-all: pct=0 → node keeps nothing.
        assert_eq!(compute_node_share(10_000, 0.0), 0);
    }

    #[test]
    fn node_share_one_pct_keeps_all() {
        // pct=1.0 → counterparty leg is empty; full amount stays with node.
        assert_eq!(compute_node_share(10_000, 1.0), 10_000);
    }

    #[test]
    fn node_share_floors_no_rounding_leak() {
        // 333 * 0.5 = 166.5 → floor 166. Counterparty gets 333 - 166 = 167.
        // The two sum exactly to amount_sats (spec §8.1 invariant).
        let share = compute_node_share(333, 0.5);
        assert_eq!(share, 166);
        assert_eq!(333 - share, 167);
    }

    #[test]
    fn node_share_zero_or_negative_amount() {
        assert_eq!(compute_node_share(0, 0.5), 0);
        assert_eq!(compute_node_share(-100, 0.5), 0);
    }

    #[test]
    fn node_share_clamps_out_of_range_pct() {
        // Config deserializer rejects out-of-range pct; clamp defensively
        // anyway so a future programmatic caller can't underflow.
        assert_eq!(compute_node_share(10_000, -0.1), 0);
        assert_eq!(compute_node_share(10_000, 1.5), 10_000);
    }
}
