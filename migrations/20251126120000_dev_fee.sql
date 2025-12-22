-- Add development fee tracking columns to orders table
-- These columns track the development fee amount, payment status, and payment hash
-- for sustainable funding of Mostro development

ALTER TABLE orders ADD COLUMN dev_fee INTEGER DEFAULT 0;
ALTER TABLE orders ADD COLUMN dev_fee_paid INTEGER NOT NULL DEFAULT 0;
ALTER TABLE orders ADD COLUMN dev_fee_payment_hash CHAR(64);