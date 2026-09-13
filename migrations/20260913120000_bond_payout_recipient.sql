-- The pubkey the counterparty share of a slashed bond is paid to, taken at
-- slash time from the order as it then stood. A waiting-state timeout
-- clears the responsible taker's pubkeys from the order (`edit_pubkeys_order`)
-- right after the slash, and the payout resolver, which read the order,
-- could no longer name the winner: the share forfeited whole to the node
-- (MOSTRO-006). NULL on rows slashed before this column existed; the
-- resolver then falls back to the order.
ALTER TABLE bonds ADD COLUMN payout_recipient char(64);
