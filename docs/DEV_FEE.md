# Development Fee Technical Specification

## Overview

The development fee mechanism provides sustainable funding for Mostro development by automatically sending a configurable percentage of the Mostro fee to a lightning address set on `DEV_FEE_LIGHTNING_ADDRESS` on each successful order.

**Key Design Principles:**
- Transparent and configurable
- Non-blocking (failures don't prevent order completion)
- Full audit trail for accountability
- Split payment model (both buyer and seller pay half)

## Architecture

### Fee Flow Diagram

```
Order Creation → Fee Calculation → Hold Invoice → Order Success → Dev Payment
     ↓                 ↓                ↓              ↓              ↓
  amount         mostro_fee        seller pays    settle hold    LNURL resolve
                     ↓             amount + fees      ↓              ↓
                 dev_fee                          buyer paid    send payment
                                                                     ↓
                                                              update db fields
```

### System Components

1. **Configuration Layer** (`src/config/`)
   - `constants.rs`: Hardcoded constraints (10-100%, Lightning Address)
   - `types.rs`: MostroSettings with dev_fee_percentage
   - `util.rs`: Startup validation

2. **Fee Calculation** (`src/util.rs`)
   - `get_dev_fee()`: Computes percentage of Mostro fee
   - `prepare_new_order()`: Calculates fees during order creation
   - `show_hold_invoice()`: Includes dev fee in seller's hold invoice

3. **Payment Execution** (`src/app/release.rs`)
   - `send_dev_fee_payment()`: LNURL resolution and payment
   - `payment_success()`: Integration point after buyer payment

4. **Database Schema** (`migrations/20251126120000_dev_fee.sql`)
   - `dev_fee`: Amount in satoshis
   - `dev_fee_paid`: Boolean (0/1)
   - `dev_fee_payment_hash`: Payment hash for reconciliation

## Configuration

### Constants (`src/config/constants.rs`)

```rust
pub const MIN_DEV_FEE_PERCENTAGE: f64 = 0.10;  // 10% minimum
pub const MAX_DEV_FEE_PERCENTAGE: f64 = 1.0;   // 100% maximum
pub const DEV_FEE_LIGHTNING_ADDRESS: &str = "<dev@lightning.address>";
```

### Settings (`~/.mostro/settings.toml`)

```toml
[mostro]
# Development sustainability fee
# Percentage of Mostro fee sent to development fund (minimum 10%)
dev_fee_percentage = 0.30
```

**Validation Rules:**
- Must be between 0.10 (10%) and 1.0 (100%)
- Validated on daemon startup
- Invalid values cause startup failure with error message

### Default Values

- `dev_fee_percentage`: 0.30 (30%)
- Configured in `src/config/types.rs::MostroSettings::default()`

## Technical Implementation

### Fee Calculation

**Function:** `src/util.rs::get_dev_fee()`

```rust
pub fn get_dev_fee(mostro_fee: i64) -> i64 {
    let mostro_settings = Settings::get_mostro();
    let dev_fee = (mostro_fee as f64) * mostro_settings.dev_fee_percentage;
    dev_fee.round() as i64
}
```

**Formula:**
```
total_dev_fee = round(total_mostro_fee × dev_fee_percentage)
buyer_dev_fee = total_dev_fee / 2
seller_dev_fee = total_dev_fee / 2
```

**Examples:**
- Total Mostro fee: 1,000 sats, Percentage: 30% → Total dev fee: 300 sats (150 buyer + 150 seller)
- Total Mostro fee: 333 sats, Percentage: 30% → Total dev fee: 100 sats (50 buyer + 50 seller, rounded)
- Mostro fee: 0 sats → Dev fee: 0 sats

### Order Creation

**Location:** `src/util.rs::prepare_new_order()` (lines 375-407)

When creating a new order:
1. Calculate Mostro fee: `fee = get_fee(amount)`
2. Calculate dev fee: `dev_fee = get_dev_fee(fee)`
3. Store in Order struct with `dev_fee_paid = false`

### Market Price Orders and Dev Fee Reset

**Critical Behavior for Market Price Orders:**

When a user takes a market price order, the following sequence occurs:

1. **Order Taken** (status changes from `pending` to `waiting-buyer-invoice` or `waiting-payment`):
   - Dev fee is calculated based on current market price
   - `order.dev_fee` field is updated with the calculated amount
   - Order waits for taker to provide invoice (sell order) or make payment (buy order)

2. **Taker Abandons Order** (doesn't provide invoice or make payment):
   - Order status is reset back to `pending`
   - **CRITICAL:** `order.dev_fee` field **MUST** be reset to `0`
   - This prevents incorrect dev fee charges if order is re-taken at different price

3. **Why Reset is Necessary:**
   - Market prices fluctuate continuously
   - Next taker may take order at different fiat amount
   - Mostro fee and dev fee must be recalculated for new amount
   - Leaving old dev_fee value would cause incorrect accounting

**Example Scenario:**

```
Initial Order: 100,000 sats at $50,000/BTC (market price)
- Mostro fee: 1,000 sats
- Dev fee (30%): 300 sats
- Order status: pending, dev_fee: 0

Taker 1 takes order at $50,000/BTC:
- Order status: waiting-buyer-invoice
- Dev fee calculated: 300 sats
- Order dev_fee: 300

Taker 1 abandons (doesn't provide invoice):
- Order status: pending
- Dev fee RESET: 0  ← CRITICAL RESET
- Order dev_fee: 0

Taker 2 takes order at $52,000/BTC (price increased):
- Order status: waiting-buyer-invoice
- Dev fee recalculated: 310 sats (new amount)
- Order dev_fee: 310

Taker 2 completes order:
- Order status: success
- Dev fee paid: 310 sats (correct amount for actual trade)
```

**Implementation Requirements:**

When order status transitions back to `pending` from any intermediate state (`waiting-buyer-invoice`, `waiting-payment`, etc.):

```rust
// Reset order state for market price orders
if order.is_market_price() {
    order.dev_fee = 0;  // Reset to zero
    order.status = Status::Pending;
    order.update(pool).await?;
}
```

**Database Consistency:**

Orders in `pending` status should always have `dev_fee = 0` for market price orders. You can verify this:

```sql
-- Should return 0 rows (all pending market orders should have dev_fee = 0)
SELECT id, premium, dev_fee
FROM orders
WHERE status = 'pending'
  AND premium IS NULL  -- market price indicator
  AND dev_fee != 0;
```

### Hold Invoice Generation

**Location:** `src/util.rs::show_hold_invoice()` (lines 651-679)

Seller's hold invoice includes seller's half of the dev fee:
```rust
let total_dev_fee = get_dev_fee(total_mostro_fee);  // 30% of total Mostro fee
let seller_dev_fee = total_dev_fee / 2;              // Seller pays half
let new_amount = order.amount + order.fee + seller_dev_fee;
```

Buyer's received amount is reduced by buyer's half of the dev fee:
```rust
let buyer_dev_fee = total_dev_fee / 2;              // Buyer pays half
buyer_receives = order.amount - buyer_fee_share - buyer_dev_fee;
```

### Example Calculation

**Order Amount**: 100,000 sats
**Mostro Fee (1%)**: 1,000 sats (split: 500 buyer + 500 seller)
**Dev Fee Percentage**: 30%
**Total Dev Fee**: 1,000 × 0.30 = 300 sats
**Dev Fee Split**: 150 sats (buyer) + 150 sats (seller)

**Seller Pays**: 100,000 + 500 + 150 = **100,650 sats**
**Buyer Receives**: 100,000 - 500 - 150 = **99,350 sats**

**Fee Distribution:**
- Buyer pays: 500 (Mostro fee) + 150 (dev fee) = **650 sats total**
- Seller pays: 500 (Mostro fee) + 150 (dev fee) = **650 sats total**
- Total dev fee collected: **300 sats** (split 50/50 between parties)

### Edge Cases

**Rounding**:
- Total: 333 sats Mostro fee × 30% = 99.9 → **100 sats total dev fee** (50 buyer + 50 seller after split)
- Total: 3 sats Mostro fee × 30% = 0.9 → **1 sat total dev fee** (split: 0 buyer + 1 seller, or round-robin)
- **Odd numbers**: When total dev fee is odd, one party pays 1 sat more (implementation decides which)

**Zero Fee Orders**:
- If `mostro_fee = 0`, then `total_dev_fee = 0`
- No dev payment attempted from either party

**Tiny Amounts**:
- Smallest: 1 sat Mostro fee × 10% = 0.1 → **0 sats** total (rounds to zero, neither party pays)
- No dev payment attempted for 0 sat dev fees

### Payment Execution

**Scheduler-Based Payment Trigger:**

The dev fee payment is **NOT** executed immediately during order release. Instead:

1. **Order Release** (`src/app/release.rs::payment_success()`):
   - Buyer receives their satoshis successfully
   - Order is marked as `status = 'success'`
   - Order is **enqueued for scheduler processing** by marking `dev_fee_paid = false`
   - Order completion notifications sent to both parties

2. **Scheduler Processing** (`src/scheduler.rs::process_dev_fee_payment()`):
   - Runs every 60 seconds
   - Queries database for orders where: `status = 'success' AND dev_fee > 0 AND dev_fee_paid = 0`
   - Processes each unpaid dev fee asynchronously
   - Timeout: 50 seconds per payment attempt

**Why Scheduler-Based?**
- **Non-blocking order completion:** Buyer receives sats immediately, dev fee payment happens asynchronously
- **Retry mechanism:** Failed payments are automatically retried on the next cycle (60 seconds)
- **Fault tolerance:** Order completes successfully even if dev fee payment fails temporarily
- **Better user experience:** Users don't wait for dev fee payment during order release

**Payment Flow (3 Steps with Timeouts):**

```rust
// [Step 1/3] LNURL resolution (15 second timeout)
let payment_request = tokio::time::timeout(
    std::time::Duration::from_secs(15),
    resolv_ln_address(DEV_FEE_LIGHTNING_ADDRESS, dev_fee_amount)
).await?;

// [Step 2/3] Create LND connector
let ln_client = LndConnector::new().await?;

// [Step 3/3] Send payment (5 second timeout for send_payment call + 25 second timeout for payment result)
let send_result = tokio::time::timeout(
    std::time::Duration::from_secs(5),
    ln_client.send_payment(&payment_request, dev_fee_amount, tx)
).await?;

// Wait for payment result (25 second timeout)
let payment_result = tokio::time::timeout(
    std::time::Duration::from_secs(25),
    rx.recv()
).await?;
```

**Total Time Budget:**
- LNURL resolution: 15s max
- send_payment call: 5s max (prevents hanging on self-payments or network issues)
- Payment result wait: 25s max
- **Total: 45s max** (under 50s scheduler timeout)

**Success Response:**
```rust
Ok(hash) => {
    order.dev_fee_paid = true;
    order.dev_fee_payment_hash = Some(hash);
    // Database updated, won't be retried
}
```

**Failure Response:**
```rust
Err(e) => {
    order.dev_fee_paid = false;
    // Logged for audit
    // Will be retried on next scheduler cycle (60 seconds)
}
```

### Error Handling

**Payment Failures:**
- LNURL resolution failure (timeout: 15 seconds)
- LND send_payment hanging (timeout: 5 seconds)
- LND connection error
- Payment routing failure
- Payment result timeout (25 seconds)
- Scheduler timeout (50 seconds total)

**Response:** All errors logged but order completes successfully. Failed payments are automatically retried on next scheduler cycle (60 seconds).

**Common Error Scenarios:**

1. **Self-Payment Attempts:** When dev fee destination uses same Lightning node as Mostro, LND may hang trying to route payment. The 5-second timeout on `send_payment()` prevents indefinite blocking.

2. **LNURL Resolution Failures:** Network issues or DNS problems resolving `DEV_FEE_LIGHTNING_ADDRESS`. 15-second timeout ensures fast failure.

3. **Routing Failures:** No route found to destination or insufficient liquidity. Payment fails after attempting routing for up to 25 seconds.

## Database Schema

### Migration: `migrations/20251126120000_dev_fee.sql`

```sql
ALTER TABLE orders ADD COLUMN dev_fee INTEGER DEFAULT 0;
ALTER TABLE orders ADD COLUMN dev_fee_paid INTEGER NOT NULL DEFAULT 0;
ALTER TABLE orders ADD COLUMN dev_fee_payment_hash CHAR(64);
```

### Field Descriptions

| Column | Type | Default | Description |
|--------|------|---------|-------------|
| `dev_fee` | INTEGER | 0 | Development fee amount in satoshis |
| `dev_fee_paid` | INTEGER | 0 | Boolean: 0 = failed/not paid, 1 = paid |
| `dev_fee_payment_hash` | CHAR(64) | NULL | Lightning payment hash for reconciliation |

### Backward Compatibility

- Existing orders: dev_fee = 0, dev_fee_paid = 0
- No migration required for existing data
- Daemon handles NULL/zero values gracefully

## Monitoring and Operations

### Statistics Queries

**Unpaid Development Fees:**
```sql
SELECT id, dev_fee, created_at, status
FROM orders
WHERE status = 'success'
  AND dev_fee > 0
  AND dev_fee_paid = 0
ORDER BY created_at DESC;
```

**Development Fee Summary:**
```sql
SELECT
  COUNT(*) as total_orders,
  SUM(dev_fee) as total_dev_fees_sats,
  SUM(CASE WHEN dev_fee_paid = 1 THEN dev_fee ELSE 0 END) as paid_sats,
  SUM(CASE WHEN dev_fee_paid = 0 THEN dev_fee ELSE 0 END) as unpaid_sats,
  ROUND(100.0 * SUM(CASE WHEN dev_fee_paid = 1 THEN 1 ELSE 0 END) / COUNT(*), 2) as success_rate
FROM orders
WHERE status = 'success' AND dev_fee > 0;
```

**Recent Failures:**
```sql
SELECT id, dev_fee, created_at
FROM orders
WHERE status = 'success'
  AND dev_fee > 0
  AND dev_fee_paid = 0
  AND created_at > strftime('%s', 'now', '-24 hours')
ORDER BY created_at DESC;
```

### Log Filtering

**View all dev fee logs:**
```bash
RUST_LOG="dev_fee=debug" mostrod
```

**View only errors:**
```bash
RUST_LOG="dev_fee=error" mostrod
```

**Log Examples:**

Success:
```
[INFO dev_fee] order_id=550e8400-e29b-41d4-a716-446655440000 amount_sats=300 destination=<dev@lightning.address> Initiating development fee payment
[INFO dev_fee] order_id=550e8400-e29b-41d4-a716-446655440000 payment_hash=abcd1234... Development fee payment succeeded
```

Failure:
```
[ERROR dev_fee] order_id=550e8400-e29b-41d4-a716-446655440000 error=LnAddressParseError stage=address_resolution Failed to resolve development Lightning Address
[ERROR dev_fee] order_id=550e8400-e29b-41d4-a716-446655440000 dev_fee=300 Development fee payment failed - order completing anyway
```

## Troubleshooting

### Common Issues

**1. Daemon Won't Start - Invalid Configuration**
```
Error: Configuration error: dev_fee_percentage (0.05) is below minimum (0.10)
```
**Solution:** Set `dev_fee_percentage` to at least 0.10 in settings.toml

**2. High Failure Rate**
- Check Lightning node connectivity
- Verify `<dev@lightning.address>` is reachable
- Check routing capacity to destination
- Review error logs: `RUST_LOG="dev_fee=error" mostrod`

**3. Payment Timeouts**
- LNURL resolution timeout: 15 seconds (indicates DNS/network issues)
- send_payment timeout: 5 seconds (indicates LND hanging, often self-payment attempts)
- Payment result timeout: 25 seconds (indicates routing issues or network congestion)
- Total scheduler timeout: 50 seconds
- Orders still complete successfully regardless of dev fee payment failures
- Failed payments automatically retry every 60 seconds via scheduler

### Manual Retry Procedure

For orders with unpaid dev fees:

1. Identify unpaid fees:
```sql
SELECT id, dev_fee FROM orders WHERE dev_fee_paid = 0 AND dev_fee > 0;
```

2. Use Lightning CLI to manually pay:
```bash
lncli payinvoice <invoice_from_lnurl>
```

3. Update database:
```sql
UPDATE orders
SET dev_fee_paid = 1, dev_fee_payment_hash = '<payment_hash>'
WHERE id = '<order_id>';
```

## Security Considerations

### Hardcoded Values

- Lightning Address: `<dev@lightning.address>` (cannot be changed without recompiling)
- Minimum fee: 10% (enforced at startup)
- Prevents misconfiguration or malicious changes

### Payment Isolation

- Dev fee payment errors don't affect core order functionality
- Failed payments logged for audit but don't halt operations
- Ensures platform reliability while maintaining transparency

### Audit Trail

- All fees recorded in database
- Payment hashes enable verification
- Logs provide forensic evidence
- Operators can reconcile payments independently

## Performance Impact

### Latency

- LNURL resolution: ~1-3 seconds (15s timeout)
- LND send_payment call: ~100-500ms (5s timeout)
- Payment execution: ~2-5 seconds (25s timeout)
- Total payment time: ~3-8 seconds typical, 45s maximum
- Scheduler processing interval: 60 seconds
- **Total order delay:** None (payment runs asynchronously via scheduler after buyer receives sats)

### Resource Usage

- Minimal CPU overhead (single calculation per order)
- Negligible memory impact
- Database: 3 additional columns per order (~76 bytes)

## Testing

### Unit Tests

Location: `src/util.rs::tests` (lines 1424-1474)

**Coverage:**
- Standard percentage calculation
- Rounding behavior
- Zero fee handling
- Small amount edge cases

**Run tests:**
```bash
cargo test test_dev_fee
```

### Integration Testing

**Manual Test Checklist:**

1. **Configuration Validation:**
   - Set `dev_fee_percentage = 0.05` → Daemon refuses to start ✓
   - Set `dev_fee_percentage = 1.5` → Daemon refuses to start ✓
   - Set `dev_fee_percentage = 0.30` → Daemon starts ✓

2. **Fee Calculation:**
   - Create 100,000 sat order with 1% Mostro fee
   - Verify seller hold invoice: 100,650 sats (100k + 500 + 150)
   - Verify buyer receives: 99,350 sats (100k - 500 - 150)
   - Verify total dev fee: 300 sats (150 from buyer + 150 from seller)

3. **Payment Flow:**
   - Complete order successfully
   - Check database: `dev_fee_paid = 1`, `dev_fee_payment_hash != NULL`
   - Verify logs show successful payment

4. **Error Handling:**
   - Simulate payment failure (disconnect Lightning node)
   - Verify order still completes with `status = 'success'`
   - Check `dev_fee_paid = 0` in database

## Migration Guide

### For Existing Installations

1. **Backup database:**
```bash
cp ~/.mostro/mostro.db ~/.mostro/mostro.db.backup
```

2. **Update Mostro:**
```bash
git pull origin main
cargo build --release
```

3. **Update settings.toml:**
```toml
[mostro]
dev_fee_percentage = 0.30  # Add this line
```

4. **Restart daemon:**
```bash
mostrod
```

5. **Verify:**
```bash
# Check settings loaded
grep "Settings correctly loaded" mostrod.log

# Check migration applied
sqlite3 ~/.mostro/mostro.db "PRAGMA table_info(orders);" | grep dev_fee
```

### Rollback Procedure

If issues arise:

1. Stop daemon
2. Restore backup: `cp ~/.mostro/mostro.db.backup ~/.mostro/mostro.db`
3. Checkout previous version: `git checkout <previous_commit>`
4. Rebuild: `cargo build --release`
5. Restart daemon

