-- `completed_trades_for_identity` (reputation export eligibility) looks an
-- identity up as master buyer or master seller of its `success` orders.
-- `idx_orders_master_seller_pubkey` already serves one side; this serves the
-- other, so the lookup reads that identity's orders and not every `success`
-- order through `idx_orders_status`.
CREATE INDEX IF NOT EXISTS idx_orders_master_buyer_pubkey ON orders(master_buyer_pubkey);
