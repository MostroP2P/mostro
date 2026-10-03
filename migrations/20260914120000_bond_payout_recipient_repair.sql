-- Repair slashed bonds stranded by MOSTRO-006 before `payout_recipient`
-- existed: a waiting-state timeout cleared the slashed taker's pubkeys from
-- the order, the order-derived resolver named nobody, and the payout skipped
-- forever. Only the unambiguous subset is filled: the order still names
-- exactly one side and that side is not the slashed bond's, so it is the
-- winner. When the remaining key is the slashed bond's own (the winner's key
-- was the one cleared) the winner is unrecoverable from the database and the
-- row is left for operator review. Maker-refund rows (`parent_bond_id` set,
-- `child_order_id` NULL) pay `bond.pubkey` and are never touched.
UPDATE bonds
   SET payout_recipient = (
       SELECT COALESCE(o.buyer_pubkey, o.seller_pubkey)
         FROM orders o
        WHERE o.id = bonds.order_id)
 WHERE payout_recipient IS NULL
   AND slashed_reason IS NOT NULL
   AND state IN ('pending-payout', 'failed')
   AND NOT (parent_bond_id IS NOT NULL AND child_order_id IS NULL)
   AND EXISTS (
       SELECT 1
         FROM orders o
        WHERE o.id = bonds.order_id
          AND (o.buyer_pubkey IS NULL) <> (o.seller_pubkey IS NULL)
          AND COALESCE(o.buyer_pubkey, o.seller_pubkey) <> bonds.pubkey);
