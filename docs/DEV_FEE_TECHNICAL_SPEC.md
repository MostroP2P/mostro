# Development Fee Technical Specification

## Overview

The development fee mechanism provides sustainable funding for Mostro development by automatically sending a configurable percentage of the Mostro fee to `development@mostro.network` on each successful order.

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
pub const DEV_FEE_LIGHTNING_ADDRESS: &str = "development@mostro.network";
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

**Location:** `src/app/release.rs::send_dev_fee_payment()` (lines 554-659)

**Flow:**
1. Verify dev_fee > 0
2. Resolve Lightning Address via LNURL: `resolv_ln_address()`
3. Create LND connector
4. Send payment with 30-second timeout
5. Return payment hash or error

**Non-blocking Design:**
```rust
match send_dev_fee_payment(order).await {
    Ok(hash) if !hash.is_empty() => {
        order.dev_fee_paid = true;
        order.dev_fee_payment_hash = Some(hash);
    }
    Err(e) => {
        order.dev_fee_paid = false;
        // Log error but continue order completion
    }
}
```

### Error Handling

**Payment Failures:**
- LNURL resolution failure
- LND connection error
- Payment routing failure
- Timeout (30 seconds)

**Response:** All errors logged with `target: "dev_fee"` but order completes successfully.

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
[INFO dev_fee] order_id=550e8400-e29b-41d4-a716-446655440000 amount_sats=300 destination=development@mostro.network Initiating development fee payment
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
- Verify `development@mostro.network` is reachable
- Check routing capacity to destination
- Review error logs: `RUST_LOG="dev_fee=error" mostrod`

**3. Payment Timeouts**
- Current timeout: 30 seconds
- May indicate routing issues or network congestion
- Orders still complete successfully

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

- Lightning Address: `development@mostro.network` (cannot be changed without recompiling)
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

- LNURL resolution: ~1-3 seconds
- Payment execution: ~2-5 seconds
- Timeout: 30 seconds maximum
- **Total order delay:** None (payment runs after order completion notification)

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

## Future Enhancements

### Potential Improvements

- Retry mechanism for failed payments
- Configurable timeout duration
- Multiple destination addresses (split funding)
- Real-time payment status dashboard
- Automatic reconciliation reports

### Not Recommended

- Making Lightning Address configurable (security risk)
- Removing minimum fee requirement (sustainability risk)
- Blocking orders on payment failure (reliability risk)

## References

- Source code: `src/config/constants.rs`, `src/util.rs`, `src/app/release.rs`
- Database migration: `migrations/20251126120000_dev_fee.sql`
- Settings template: `settings.tpl.toml`
- Unit tests: `src/util.rs::tests::test_dev_fee_*`

## Support

For issues or questions:
- GitHub: https://github.com/MostroP2P/mostro/issues
- Lightning Address verification: https://lightningaddress.com/
