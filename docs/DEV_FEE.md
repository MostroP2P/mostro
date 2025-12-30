# Development Fee Technical Specification

## Overview

The development fee mechanism provides sustainable funding for Mostro development by automatically sending a configurable percentage of the Mostro fee to a lightning address set on `DEV_FEE_LIGHTNING_ADDRESS` on each successful order.

**Key Design Principles:**
- Transparent and configurable
- Non-blocking (failures don't prevent order completion)
- Full audit trail for accountability
- Split payment model (both buyer and seller pay half)

## Implementation Status

### Phase 1: Infrastructure ✅ COMPLETE

**What's Implemented:**
- Configuration constants (MIN/MAX percentages, Lightning address) in `src/config/constants.rs`
- Settings validation on daemon startup in `src/config/util.rs`
- Database schema with 3 columns: `dev_fee`, `dev_fee_paid`, `dev_fee_payment_hash`
- Database fields initialized in order creation (currently hardcoded to 0)
- Default dev_fee_percentage: 0.30 (30%)

**Status:** Ready for Phase 2 implementation

### Phase 2: Fee Calculation ✅ COMPLETE

**Implemented Components:**
- `calculate_dev_fee()` pure function in `src/util.rs` (lines 167-170)
- `get_dev_fee()` wrapper function in `src/util.rs` (lines 176-179)
- Proper dev_fee split calculation (seller = dev_fee/2, buyer = dev_fee - seller_dev_fee)
- Integration in message amount calculations across 4 critical locations
- Unit tests for fee calculation logic (4 tests passing)

**Implementation Details:**
- Two-function approach: Pure calculation function + Settings wrapper
- Prevents satoshi loss with odd dev_fee amounts
- Buyer pays extra satoshi when dev_fee is odd
- All fee calculations include dev_fee in buyer/seller amounts

**Status:** ✅ Complete - Fee calculations implemented and tested across all order flows

### Phase 3: Payment Execution ✅ COMPLETE

**Implemented Components:**
- `send_dev_fee_payment()` function in `src/app/release.rs` (lines 564-678)
- Scheduler job `process_dev_fee_payment()` in `src/scheduler.rs` (lines 511-608)
- Database query function `find_unpaid_dev_fees()` in `src/db.rs` (lines 895-908)
- **Database field updates:**
  - `dev_fee`: Set during order creation, reset to 0 for abandoned market orders, calculated for market price orders
  - `dev_fee_paid`: Changed from 0 to 1 after successful payment in scheduler
  - `dev_fee_payment_hash`: Set to Lightning payment hash on successful payment
- Error handling and retry logic with automatic retries every 60 seconds
- Payment timeout handling (LNURL: 15s, send_payment: 5s, result: 25s, total: 50s per attempt)
- Dev fee amount validation (rejects payments with dev_fee <= 0)
- Enhanced logging with BEFORE/AFTER state tracking and database verification
- Race condition handling (query checks both 'settled-hold-invoice' AND 'success' statuses)
- Market price order dev_fee calculation in `take_buy.rs` and `take_sell.rs`

**Status:** ✅ Complete - Automated dev fee payment system fully operational

**Implementation Commits:**
- f508669: feat: implement automated development fee payment system
- 102cfed: fix: resolve dev fee payment failures with two critical fixes
- eaf3319: Validate dev_fee amount before attempting payment
- 42253f1: Fix edge case on dev fund payment
- 2655943: fix: improve dev fee payment reliability and coverage

### Phase 4: Audit Events via Nostr ⚠️ TO IMPLEMENT

**What Will Be Implemented:**
- Custom Nostr event kind (38383) for dev fee payment audits
- Event publishing in scheduler after successful payment
- Public relay distribution for third-party verification and total tracking
- Complete payment details: order_id, dev_fee, payment_hash, timestamp

**Status:** ⚠️ TO IMPLEMENT - Planned for future release (see Phase 4 section for full specification)

### Implementation Notes

This section documents key improvements and fixes made during Phase 3 implementation.

**Commit f508669 - Initial Implementation:**
- Implemented `send_dev_fee_payment()` with 4-step payment flow (validation, LNURL, send, wait)
- Created `job_process_dev_fee_payment()` scheduler job running every 60 seconds
- Added `find_unpaid_dev_fees()` database query function
- Established automatic retry mechanism for failed payments

**Commit 102cfed - Race Condition Fix:**
- **Problem:** Original query only checked `status = 'success'`, missing orders stuck in `settled-hold-invoice`
- **Solution:** Updated query to check BOTH statuses: `WHERE (status = 'settled-hold-invoice' OR status = 'success')`
- **Impact:** Handles edge case where buyer payment succeeds while dev fee payment is processing
- **Result:** Dev fee payment now triggers immediately when seller releases, not just after buyer payment completes

**Commit eaf3319 - Payment Validation:**
- **Problem:** Function could attempt payment with `dev_fee = 0` or negative values
- **Solution:** Added validation check at function start: `if order.dev_fee <= 0 { return Err(...) }`
- **Impact:** Prevents unnecessary LNURL resolution and payment attempts
- **Result:** Cleaner error handling and faster failure for invalid amounts

**Commit 42253f1 - Edge Case Fix:**
- Fixed specific edge case in dev fund payment flow
- Improved error handling for corner cases in payment processing

**Commit 2655943 - Reliability & Coverage Improvements:**
- **Market Price Orders:**
  - Added dev_fee calculation in `take_buy.rs` and `take_sell.rs`
  - Formula: `dev_fee = get_dev_fee(fee * 2)` when order amount is determined
  - Fixes bug where market price orders permanently had `dev_fee = 0`
- **Enhanced Logging:**
  - Logs BEFORE state: `order_id, dev_fee_paid, dev_fee_payment_hash` before update
  - Logs AFTER state: Modified values before database write
  - Logs database update result: Success/failure with details
  - Logs verification query: Re-queries database to confirm persistence
  - Example output:
    ```
    BEFORE UPDATE: order_id=abc123, dev_fee_paid=false, dev_fee_payment_hash=None
    AFTER MODIFY: order_id=abc123, dev_fee_paid=true, dev_fee_payment_hash=Some("a1b2c3...")
    ✅ DATABASE UPDATE SUCCEEDED for order abc123
    VERIFICATION: order_id=abc123, dev_fee_paid=true, dev_fee_payment_hash=Some("a1b2c3...")
    ```
- **Result:** Complete diagnostic trail for debugging payment issues and database persistence

**Key Design Decisions:**

1. **Scheduler-Based vs Inline Payment:**
   - Chose scheduler-based to avoid blocking order completion
   - 60-second interval balances responsiveness with resource usage
   - Automatic retry mechanism handles transient failures

2. **Dual Status Query:**
   - Critical for handling race conditions
   - Ensures dev fee is collected even if buyer payment completes during dev fee payment
   - Prevents revenue loss from timing edge cases

3. **Enhanced Logging:**
   - BEFORE/AFTER/VERIFY pattern provides complete audit trail
   - Essential for debugging database persistence issues
   - Helps identify race conditions and timing problems

4. **Validation Before Payment:**
   - Fail-fast approach for invalid amounts
   - Reduces unnecessary network calls
   - Clearer error messages for debugging

## Architecture

### Fee Flow Diagram

```
Order Creation → Fee Calculation → Hold Invoice → Seller Release → Dev Payment → Buyer Payment
     ↓                 ↓                ↓              ↓                ↓             ↓
  amount         mostro_fee        seller pays    settle hold     LNURL resolve   buyer paid
                     ↓             amount + fees      ↓                ↓             ↓
                 dev_fee                          status=settled   send payment   status=success
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

**Current State:** ✅ IMPLEMENTED - Two functions provide fee calculation with proper satoshi handling.

**Implementation:**

Two-function approach in `src/util.rs`:

1. **Pure calculation function** (lines 167-170):
```rust
/// Pure function for calculating dev fee - useful for testing
pub fn calculate_dev_fee(total_mostro_fee: i64, percentage: f64) -> i64 {
    let dev_fee = (total_mostro_fee as f64) * percentage;
    dev_fee.round() as i64
}
```

2. **Settings wrapper** (lines 176-179):
```rust
/// Wrapper that uses configured dev_fee_percentage from Settings
pub fn get_dev_fee(total_mostro_fee: i64) -> i64 {
    let mostro_settings = Settings::get_mostro();
    calculate_dev_fee(total_mostro_fee, mostro_settings.dev_fee_percentage)
}
```

**Benefits of Two-Function Approach:**
- `calculate_dev_fee()` is pure, testable without global config
- `get_dev_fee()` provides convenient access using Settings
- Tests can verify behavior with different percentages
- Production code uses Settings seamlessly

**Formula Specification:**
```
total_dev_fee = round(total_mostro_fee × dev_fee_percentage)
seller_dev_fee = total_dev_fee / 2  (integer division, rounds down for odd amounts)
buyer_dev_fee = total_dev_fee - seller_dev_fee  (gets remainder, prevents satoshi loss)
```

**Why This Formula:**
- Integer division of odd numbers loses 1 satoshi if both parties use `dev_fee / 2`
- Our approach: seller pays rounded-down half, buyer pays remainder
- Ensures full dev_fee is always collected with no satoshi loss
- Buyer pays extra 1 sat when dev_fee is odd (fair distribution)

**Examples:**
- Total Mostro fee: 1,000 sats, Percentage: 30% → Total dev fee: 300 sats (150 buyer + 150 seller) ✓
- Total Mostro fee: 1,003 sats, Percentage: 30% → Total dev fee: 301 sats (151 buyer + 150 seller) ✓
- Total Mostro fee: 333 sats, Percentage: 30% → Total dev fee: 100 sats (50 buyer + 50 seller) ✓
- Mostro fee: 0 sats → Dev fee: 0 sats ✓

### Order Creation

**Current State:** ✅ IMPLEMENTED - Orders are created with calculated `dev_fee` based on order amount. Database fields `dev_fee_paid` and `dev_fee_payment_hash` are initialized to `false` and `None` respectively.

**Implementation:** `src/util.rs::prepare_new_order()`

When creating a new order:
1. Calculate Mostro fee: `fee = get_fee(amount)`
2. Calculate dev fee: `dev_fee = get_dev_fee(fee * 2)` (multiply by 2 for total Mostro fee from both parties)
3. Store in Order struct with `dev_fee_paid = false` and `dev_fee_payment_hash = None`

**Special Case - Market Price Orders:**
- Orders created with `amount = 0` will have `dev_fee = 0` initially
- Dev fee is calculated when order is taken (see "Market Price Order Dev Fee Calculation" section)
- Implemented in `take_buy.rs` and `take_sell.rs` (commit 2655943)

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

### Market Price Order Dev Fee Calculation

**Implementation Status:** ✅ IMPLEMENTED (Commit 2655943)

**Critical Implementation Detail:**

When a market price order is taken, three fields must be updated atomically:

**Locations:**
- `/home/negrunch/dev/mostro/src/app/take_buy.rs` (lines 52-64)
- `/home/negrunch/dev/mostro/src/app/take_sell.rs` (lines 104-115)

**Actual Implementation (take_buy.rs and take_sell.rs):**
```rust
if order.has_no_amount() {
    match get_market_amount_and_fee(order.fiat_amount, &order.fiat_code, order.premium).await {
        Ok(amount_fees) => {
            // 1. Update amount (from market price)
            order.amount = amount_fees.0;

            // 2. Update fee (calculated from amount)
            order.fee = amount_fees.1;

            // 3. Calculate and update dev_fee (calculated from fee)
            // IMPLEMENTED in commit 2655943
            let total_mostro_fee = order.fee * 2;
            order.dev_fee = get_dev_fee(total_mostro_fee);
        }
        Err(_) => return Err(MostroInternalErr(ServiceError::WrongAmountError)),
    };
}
```

**Why This Is Critical:**

1. **Timing:** All three fields (`amount`, `fee`, `dev_fee`) must be updated when order is taken
2. **Dependencies:** `dev_fee` depends on `fee`, which depends on `amount`
3. **Atomicity:** Updates happen together before order proceeds to next status
4. **Consistency:** Ensures market price orders have correct dev_fee for payment collection

**Example Scenario:**

```
Market Price Order Created:
- Fiat: $100 USD
- BTC Price: $50,000
- amount: 0 (unknown until taken)
- fee: 0
- dev_fee: 0
- Status: pending

Order Taken:
- Market Price Lookup: $100 @ $50,000 = 200,000 sats
- amount: 200,000 sats           ← Updated
- fee: 2,000 sats (1%)           ← Updated
- total_mostro_fee: 4,000 sats (both parties)
- dev_fee: 1,200 sats (30%)      ← Updated
- Status: waiting-buyer-invoice

Order Completes:
- Dev fee payment triggered: 1,200 sats sent to dev fund ✓
```

**Without This Fix:**
```
Order Taken:
- amount: 200,000 sats  ✓
- fee: 2,000 sats       ✓
- dev_fee: 0            ✗ BUG - stays 0

Order Completes:
- Dev fee payment: NOT triggered (query requires dev_fee > 0)
- Revenue loss: 1,200 sats not collected ✗
```

### Taker Abandonment and Order Reset

**Implementation Status:** ✅ IMPLEMENTED

**Critical Behavior:** When a taker abandons an order (doesn't proceed and order times out), all price-dependent fields must be reset to ensure correct recalculation when the order is re-taken.

**Two Reset Paths:**

1. **Explicit Cancellation** (`src/app/cancel.rs::reset_api_quotes()` lines 18-25):
   - Taker explicitly calls cancel action
   - Resets: `amount = 0`, `fee = 0`, `dev_fee = 0`
   - Status: ✅ Implemented

2. **Automatic Timeout** (`src/scheduler.rs::job_cancel_orders()` lines 354-358):
   - Scheduler detects taker hasn't proceeded within `expiration_seconds`
   - Resets: `amount = 0`, `fee = 0`, `dev_fee = 0`
   - Status: ✅ Implemented

**Why All Three Must Reset:**
- Market price can change between takes
- Fee is calculated from amount
- Dev fee is calculated from fee
- Leaving stale `dev_fee` value causes incorrect charges on re-take

**Example Flow:**
```
Order Created (market price):
- amount: 0, fee: 0, dev_fee: 0, status: pending

Order Taken at BTC=$50,000:
- amount: 200,000 sats, fee: 2,000 sats, dev_fee: 600 sats
- status: waiting-buyer-invoice

Taker Abandons (timeout after expiration_seconds):
- Scheduler detects timeout (taken_at > expiration_seconds)
- Resets: amount: 0, fee: 0, dev_fee: 0
- status: pending (ready for new taker)

Order Re-taken at BTC=$52,000 (price increased):
- amount: 192,308 sats, fee: 1,923 sats, dev_fee: 577 sats
- Correct dev_fee for new market price ✓
```

**Implementation Details:**
- `src/app/cancel.rs` lines 18-25: `reset_api_quotes()` function
- `src/scheduler.rs` lines 354-358: Automatic timeout handler in `job_cancel_orders()`
- Both paths use same logic:
  ```rust
  if order.price_from_api {
      order.amount = 0;
      order.fee = 0;
      order.dev_fee = 0;
  }
  ```
- Database function: `update_order_to_initial_state()` persists the reset values

**Database Persistence Fix:**

Prior to commit `c803471`, the `update_order_to_initial_state()` function in `src/db.rs`
did not include `dev_fee` in its SQL UPDATE statement, causing stale dev_fee values to
remain in the database even though the in-memory Order struct had `dev_fee = 0`.

**Before Fix:**
- Memory: `order.dev_fee = 0` ✓
- Database: `dev_fee = 300` (stale) ✗
- After `edit_pubkeys_order()` fetches from DB: `order.dev_fee = 300` (wrong!) ✗

**After Fix:**
- Memory: `order.dev_fee = 0` ✓
- Database: `dev_fee = 0` ✓
- After `edit_pubkeys_order()` fetches from DB: `order.dev_fee = 0` ✓

The fix added `dev_fee` as a parameter to `update_order_to_initial_state()` and
included it in the SQL UPDATE statement, ensuring the value is properly persisted
to the database.

**Configuration:**
- Timeout duration: Configured via `expiration_seconds` in settings
- Default: Orders return to pending after taker hasn't proceeded for configured time

### Hold Invoice Generation

**Current State:** ✅ IMPLEMENTED - Hold invoices include seller's half of the dev fee.

**Implementation:** `src/util.rs::show_hold_invoice()` (lines 660-675)

Seller's hold invoice includes seller's half of the dev fee:
```rust
// Seller pays their half of the dev fee (order.dev_fee stores total)
// Integer division rounds down, so seller gets the smaller half for odd amounts
let seller_dev_fee = order.dev_fee / 2;
let new_amount = order.amount + order.fee + seller_dev_fee;

// Now we generate the hold invoice that seller should pay
let (invoice_response, preimage, hash) = ln_client...
```

**Key Points:**
- `order.dev_fee` stores the total dev fee for the order (both buyer and seller combined)
- Seller pays `dev_fee / 2` (integer division rounds down for odd amounts)
- Buyer pays `dev_fee - seller_dev_fee` (gets the remainder, see Message Amount Calculations section)
- Hold invoice amount = `order.amount + order.fee + seller_dev_fee`

### Message Amount Calculations

**Implementation Status:** ✅ COMPLETE

When creating messages for buyers and sellers during order flow, the amounts must include proper dev_fee calculations. The implementation ensures correct amounts are communicated to both parties.

**Seller Messages:**
```rust
let seller_dev_fee = order.dev_fee / 2;
seller_order.amount = order.amount
    .saturating_add(order.fee)
    .saturating_add(seller_dev_fee);
```

**Buyer Messages:**
```rust
let seller_dev_fee = order.dev_fee / 2;
let buyer_dev_fee = order.dev_fee - seller_dev_fee;
buyer_order.amount = order.amount
    .saturating_sub(order.fee)
    .saturating_sub(buyer_dev_fee);
```

**Critical Implementation Locations:**

1. **`src/flow.rs::hold_invoice_paid()`** (lines 54-75)
   - Purpose: Status updates after seller payment
   - Seller amount: Includes `seller_dev_fee` (line 62-64)
   - Buyer amount: Subtracts `buyer_dev_fee` (line 72-75)
   - Impact: Initial payment confirmation messages

2. **`src/app/add_invoice.rs::add_invoice_action()`** (lines 88-109)
   - Purpose: Invoice acceptance flow
   - Seller amount: Includes `seller_dev_fee` (line 90-93)
   - Buyer amount: Subtracts `buyer_dev_fee` (line 105-109)
   - Impact: Order acceptance notifications

3. **`src/app/release.rs::check_failure_retries()`** (lines 70-75)
   - Purpose: Payment failure handling
   - Buyer amount: Subtracts `buyer_dev_fee` (line 72-75)
   - Impact: Failure notification amounts

4. **`src/app/release.rs::do_payment()`** ⚠️ **CRITICAL** (lines 443-448)
   - Purpose: Actual Lightning payment calculation
   - Payment amount: Subtracts `buyer_dev_fee` (line 446-448)
   - Impact: **Real sats transferred** to buyer via Lightning
   - Why critical: Determines actual payment amount, not just messages

**Implementation Pattern:**

All locations follow the same pattern for consistency:
```rust
// Calculate split to avoid satoshi loss
let seller_dev_fee = order.dev_fee / 2;
let buyer_dev_fee = order.dev_fee - seller_dev_fee;

// Apply to amounts based on party
seller_amount = order.amount + order.fee + seller_dev_fee;
buyer_amount = order.amount - order.fee - buyer_dev_fee;
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
- **Odd numbers**: When total dev fee is odd, buyer always pays 1 sat more
- Example: 301 sats total → seller: 150, buyer: 151 (total: 301 ✓)
- Formula: `seller_dev_fee = dev_fee / 2`, `buyer_dev_fee = dev_fee - seller_dev_fee`
- Reason: Ensures full fee collection with no satoshi loss
- Implementation: `src/util.rs`, `src/flow.rs`, `src/app/add_invoice.rs`, `src/app/release.rs`

**Zero Fee Orders**:
- If `mostro_fee = 0`, then `total_dev_fee = 0`
- No dev payment attempted from either party

**Tiny Amounts**:
- Smallest: 1 sat Mostro fee × 10% = 0.1 → **0 sats** total (rounds to zero, neither party pays)
- No dev payment attempted for 0 sat dev fees

### Payment Execution

**Current State:** ✅ IMPLEMENTED - Automated dev fee payment system fully operational with scheduler-based processing.

**Implementation:**

**Scheduler-Based Payment Trigger:**

The dev fee payment should **NOT** be executed immediately during order release. Instead:

1. **Order Release** (`src/app/release.rs::release_action()`):
   - Seller's hold invoice is settled
   - Order is marked as `status = 'settled-hold-invoice'`
   - Order is **enqueued for scheduler processing** by marking `dev_fee_paid = false`
   - Mostro then proceeds to pay buyer's invoice
   - **Key Point:** Dev fee payment happens asynchronously AFTER seller releases but BEFORE buyer payment completes

2. **Scheduler Processing** (`src/scheduler.rs::job_process_dev_fee_payment()` lines 511-608):
   - Runs every 60 seconds
   - Uses `find_unpaid_dev_fees()` to query database for orders where: `(status = 'settled-hold-invoice' OR status = 'success') AND dev_fee > 0 AND dev_fee_paid = 0`
   - **Important:** Query checks BOTH statuses to handle race conditions where buyer payment succeeds during dev fee payment
   - Processes each unpaid dev fee asynchronously with 50-second timeout per payment attempt
   - Enhanced logging: Logs BEFORE/AFTER state, database update results, and verification queries

**Why This Timing?**
- **Fee Earned:** Seller has released funds, so Mostro has earned its fee
- **Risk Mitigation:** Captures dev fees even if buyer payment fails permanently
- **Non-blocking:** Order flow continues regardless of dev fee payment status
- **Retry mechanism:** Failed payments are automatically retried every 60 seconds

**Why Scheduler-Based?**
- **Non-blocking order completion:** Seller release and buyer payment happen immediately, dev fee payment happens asynchronously
- **Retry mechanism:** Failed payments are automatically retried on the next cycle (60 seconds)
- **Fault tolerance:** Order completes successfully even if dev fee payment fails temporarily
- **Better user experience:** Users don't wait for dev fee payment during order release

**Payment Flow Specification (4 Steps with Timeouts):**

Implementation in `src/app/release.rs::send_dev_fee_payment()` (lines 564-678):

```rust
// [Step 0] Validation - Reject invalid amounts
if order.dev_fee <= 0 {
    return Err(MostroInternalErr(ServiceError::WrongAmountError));
}

// [Step 1/4] LNURL resolution (15 second timeout)
let payment_request = tokio::time::timeout(
    std::time::Duration::from_secs(15),
    resolv_ln_address(DEV_FEE_LIGHTNING_ADDRESS, dev_fee_amount)
).await?;

// [Step 2/4] Create LND connector
let ln_client = LndConnector::new().await?;

// [Step 3/4] Send payment (5 second timeout for send_payment call + 25 second timeout for payment result)
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
- Validation: Instant (< 1ms)
- LNURL resolution: 15s max
- send_payment call: 5s max (prevents hanging on self-payments or network issues)
- Payment result wait: 25s max
- **Total: ~45s max** (under 50s scheduler timeout)

**Validation Note:** The function validates `dev_fee > 0` before attempting payment. This was added in commit eaf3319 to prevent unnecessary payment attempts for orders with zero dev fees.

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

### Database Field Updates

This section details exactly when and how the database fields (`dev_fee`, `dev_fee_paid`, `dev_fee_payment_hash`) are modified throughout the order lifecycle.

#### `dev_fee` Field Lifecycle

**When Set:** During order creation in `src/util.rs::prepare_new_order()` (lines 392-398)

**Calculation:**
```rust
let mut fee = 0;
let mut dev_fee = 0;
if new_order.amount > 0 {
    fee = get_fee(new_order.amount);                    // Calculate Mostro fee
    let total_mostro_fee = fee * 2;                     // Double the fee (both parties)
    dev_fee = get_dev_fee(total_mostro_fee);            // Calculate dev fee (30% default)
}
```

**Initial Value:** Calculated dev fee amount in satoshis based on `total_mostro_fee × dev_fee_percentage`

**Special Case - Market Price Orders:** When a market price order returns to `pending` status (taker abandons), the `dev_fee` field **MUST** be reset to `0` to allow recalculation at the new market price when re-taken. This is documented in detail in the "Market Price Orders and Dev Fee Reset" section (lines 182-256).

**Database State:** Persists throughout order lifecycle unless order returns to pending status (market price orders only).

#### `dev_fee_paid` Field Updates

**Initial Value:** `0` (false) - Set during order creation in `src/util.rs::prepare_new_order()` (line 413)

**When Changed to `1` (true):** After successful dev fee payment in the scheduler job `process_dev_fee_payment()` in `src/scheduler.rs`

**Trigger Sequence:**
1. Seller releases order (`status = 'settled-hold-invoice'`)
2. Scheduler runs every 60 seconds
3. Query identifies unpaid orders: `SELECT * FROM orders WHERE (status = 'settled-hold-invoice' OR status = 'success') AND dev_fee > 0 AND dev_fee_paid = 0`
4. Scheduler calls `send_dev_fee_payment()` for each unpaid order
5. On payment success, scheduler updates: `order.dev_fee_paid = true` (stored as `1` in database)
6. Important: Query includes BOTH statuses to handle race conditions where buyer payment completes during dev fee payment failure

**Database Update:**
```sql
UPDATE orders
SET dev_fee_paid = 1, dev_fee_payment_hash = ?
WHERE id = ?
```

**Timing:** Asynchronously after order completes, typically within 60 seconds (next scheduler cycle)

**Remains `0` When:**
- Payment hasn't been attempted yet (order just completed)
- Payment failed (LNURL resolution error, routing failure, timeout)
- Will retry on next scheduler cycle (60 seconds)

#### `dev_fee_payment_hash` Field Updates

**Initial Value:** `NULL` - Set during order creation in `src/util.rs::prepare_new_order()` (line 414)

**When Set:** Simultaneously with `dev_fee_paid = 1` after successful payment

**Value Source:** Lightning payment hash returned from the Lightning Network payment result. The hash comes from the `ln_client.send_payment()` call's result channel in `src/app/release.rs::send_dev_fee_payment()`.

**Payment Hash Retrieval Flow:**
```rust
// Step 1: LNURL resolution (15s timeout)
let payment_request = resolv_ln_address(DEV_FEE_LIGHTNING_ADDRESS, dev_fee_amount).await?;

// Step 2: Send payment (5s timeout for send, 25s for result)
let payment_result = ln_client.send_payment(&payment_request, dev_fee_amount, tx).await?;

// Step 3: Extract hash from successful payment result
let payment_hash = rx.recv().await?;  // ← THIS is what goes into dev_fee_payment_hash
```

**Format:** 64-character hexadecimal string (standard Lightning payment hash)

**Purpose:**
- Audit trail for reconciliation
- Proof of payment for accountability
- Debugging and payment verification

**Remains `NULL` When:**
- Payment hasn't been attempted yet
- Payment failed (no hash generated for failed payments)

#### Payment Flow Timeline

Complete timeline showing database field states at each stage:

```
Order Creation (t=0):
  └─> dev_fee = calculated_value (e.g., 300 sats)
  └─> dev_fee_paid = 0
  └─> dev_fee_payment_hash = NULL
  └─> Database: INSERT INTO orders (dev_fee, dev_fee_paid, dev_fee_payment_hash) VALUES (300, 0, NULL)

Order Processing (t=minutes):
  └─> Buyer and seller complete trade
  └─> Dev fee fields remain unchanged
  └─> Database: No updates to dev_fee fields

Order Release (t=seller_release):
  └─> Seller's hold invoice settled
  └─> status = 'settled-hold-invoice'
  └─> Dev fee fields unchanged (dev_fee_paid = 0)
  └─> Order enters scheduler queue
  └─> Buyer payment initiated (asynchronous)

Scheduler Cycle (t=next_60s_cycle, typically within 60s of seller release):
  └─> Scheduler wakes up every 60 seconds
  └─> Query: SELECT * FROM orders WHERE (status = 'settled-hold-invoice' OR status = 'success') AND dev_fee > 0 AND dev_fee_paid = 0
  └─> Order found in unpaid queue
  └─> Call: send_dev_fee_payment(order)

Payment Attempt (t=payment_start):
  └─> Step 1/3: LNURL resolution (timeout: 15s)
  └─> Step 2/3: LND send_payment call (timeout: 5s)
  └─> Step 3/3: Wait for payment result (timeout: 25s)

Dev Fee Payment Success (t=payment_complete, ~3-8 seconds typical):
  └─> Payment hash received: "a1b2c3d4e5f6..."
  └─> dev_fee_paid = 1
  └─> dev_fee_payment_hash = "a1b2c3d4e5f6..."
  └─> Database: UPDATE orders SET dev_fee_paid = 1, dev_fee_payment_hash = 'a1b2c3d4e5f6...' WHERE id = ?
  └─> Order removed from retry queue
  └─> DONE ✓

Order Success (t=order_complete, after dev fee payment):
  └─> status = 'success'
  └─> Buyer receives satoshis
  └─> Dev fee already paid (dev_fee_paid = 1)
  └─> Database: UPDATE orders SET status = 'success' WHERE id = ?

Dev Fee Payment Failure (t=payment_timeout, could be 15s, 25s, or 50s timeout):
  └─> Error logged (LNURL failure, routing failure, timeout, etc.)
  └─> dev_fee_paid = 0 (unchanged)
  └─> dev_fee_payment_hash = NULL (unchanged)
  └─> Database: No update (fields remain unchanged)
  └─> Order remains in retry queue
  └─> Retry on next scheduler cycle (60 seconds later)
  └─> Will retry indefinitely until payment succeeds

Edge Case - Buyer Payment Fails:
  └─> Order remains in status = 'settled-hold-invoice'
  └─> failed_payment = true
  └─> Retry scheduler (job_retry_failed_payments) attempts buyer payment again
  └─> Dev fee already paid regardless (dev_fee_paid = 1)
  └─> This is correct: seller released, fee was earned

Edge Case - Dev Fee Payment Fails, Buyer Payment Succeeds (Race Condition):
  └─> Seller releases → status = 'settled-hold-invoice'
  └─> Scheduler attempts dev fee payment → FAILS (dev_fee_paid = 0)
  └─> Simultaneously, buyer payment → SUCCEEDS → status = 'success'
  └─> Order now in 'success' status with dev_fee_paid = 0
  └─> Query includes both statuses, so order is still picked up on next cycle
  └─> Dev fee payment eventually succeeds and updates dev_fee_paid = 1
```

#### Actual Implementation

The actual implementation in `src/scheduler.rs::job_process_dev_fee_payment()` (lines 511-608) includes enhanced logging:

```rust
/// Process unpaid development fees for successful orders
/// Called every 60 seconds by scheduler
async fn job_process_dev_fee_payment() {
    let pool = get_db_pool();
    let interval = 60u64; // Every 60 seconds

    tokio::spawn(async move {
        loop {
            info!("Checking for unpaid development fees");

            // Query unpaid orders using find_unpaid_dev_fees()
            // Query: WHERE (status = 'settled-hold-invoice' OR status = 'success')
            //        AND dev_fee > 0 AND dev_fee_paid = 0
            if let Ok(unpaid_orders) = find_unpaid_dev_fees(&pool).await {
                info!("Found {} orders with unpaid dev fees", unpaid_orders.len());

                for mut order in unpaid_orders {
                    // Attempt payment with 50-second timeout (under 60s cycle)
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(50),
                        send_dev_fee_payment(&order),
                    )
                    .await
                    {
                        Ok(Ok(payment_hash)) => {
                            // SUCCESS: Update both fields atomically
                            let order_id = order.id;
                            let dev_fee_amount = order.dev_fee;

                            // Enhanced logging - BEFORE state
                            info!(
                                "BEFORE UPDATE: order_id={}, dev_fee_paid={}, dev_fee_payment_hash={:?}",
                                order_id, order.dev_fee_paid, order.dev_fee_payment_hash
                            );

                            order.dev_fee_paid = true;
                            order.dev_fee_payment_hash = Some(payment_hash.clone());

                            // Enhanced logging - AFTER modification
                            info!(
                                "AFTER MODIFY: order_id={}, dev_fee_paid={}, dev_fee_payment_hash={:?}",
                                order_id, order.dev_fee_paid, order.dev_fee_payment_hash
                            );

                            match order.update(&pool).await {
                                Err(e) => {
                                    error!(
                                        "❌ DATABASE UPDATE FAILED for order {}: {:?}",
                                        order_id, e
                                    );
                                }
                                Ok(_) => {
                                    info!("✅ DATABASE UPDATE SUCCEEDED for order {}", order_id);

                                    // Verification query - confirm database persistence
                                    if let Ok(verified_order) = sqlx::query_as::<_, Order>(
                                        "SELECT * FROM orders WHERE id = ?",
                                    )
                                    .bind(order_id)
                                    .fetch_one(&*pool)
                                    .await
                                    {
                                        info!(
                                            "VERIFICATION: order_id={}, dev_fee_paid={}, dev_fee_payment_hash={:?}",
                                            verified_order.id,
                                            verified_order.dev_fee_paid,
                                            verified_order.dev_fee_payment_hash
                                        );
                                    }

                                    info!(
                                        "Dev fee payment succeeded for order {} - amount: {} sats, hash: {}",
                                        order_id, dev_fee_amount, payment_hash
                                    );
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            // FAILURE: Leave fields unchanged for retry
                            error!(
                                "Dev fee payment failed for order {} ({} sats) - error: {:?}, will retry",
                                order.id, order.dev_fee, e
                            );
                        }
                        Err(_) => {
                            // TIMEOUT: Leave fields unchanged for retry
                            error!(
                                "Dev fee payment timeout (50s) for order {} ({} sats) - will retry",
                                order.id, order.dev_fee
                            );
                        }
                    }
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}
```

**Key Implementation Points:**

1. **Atomic Updates:** Always update `dev_fee_paid` and `dev_fee_payment_hash` together in a single database transaction
2. **Only Update on Success:** Never update these fields on payment failure - leave them unchanged for retry
3. **Dual Status Query:** Query checks BOTH `'settled-hold-invoice'` AND `'success'` statuses to handle race conditions
4. **Enhanced Logging:** BEFORE/AFTER/VERIFY pattern provides complete diagnostic trail
5. **Database Verification:** After update, re-query database to confirm fields were persisted correctly
6. **Automatic Retry:** Failed payments remain with `dev_fee_paid = 0`, causing them to be retried on the next cycle (60s)
7. **Non-Blocking:** Order completion is never blocked by dev fee payment attempts
8. **Error Categorization:** Separate handling for payment failures vs timeouts with specific error messages

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

## Implementation Roadmap

This section provides a checklist for implementing the remaining phases of the development fee feature.

### Phase 2: Fee Calculation ✅ COMPLETE

**Prerequisites:** Phase 1 complete ✅

**Implementation Tasks:**
- [x] Implement `calculate_dev_fee()` pure function in `src/util.rs`
  - Input: `total_mostro_fee: i64, percentage: f64`
  - Output: `i64` (rounded dev fee amount)
  - Logic: `(total_mostro_fee as f64) * percentage`, rounded
  - Location: Lines 167-170
- [x] Implement `get_dev_fee()` wrapper function in `src/util.rs`
  - Input: `total_mostro_fee: i64`
  - Output: `i64` (calls calculate_dev_fee with Settings percentage)
  - Location: Lines 176-179
- [x] Implement proper dev_fee split (avoid satoshi loss)
  - Formula: `seller_dev_fee = dev_fee / 2`, `buyer_dev_fee = dev_fee - seller_dev_fee`
  - Ensures full fee collection with odd amounts
- [x] Update message creation in `src/flow.rs::hold_invoice_paid()`
  - Lines 54-56: Calculate proper dev_fee split
  - Lines 62-64: Seller amount includes seller_dev_fee
  - Lines 72-75: Buyer amount subtracts buyer_dev_fee
- [x] Update message creation in `src/app/add_invoice.rs::add_invoice_action()`
  - Lines 88-93: Seller amount includes seller_dev_fee
  - Lines 103-109: Buyer amount subtracts buyer_dev_fee
- [x] Update payment calculation in `src/app/release.rs::check_failure_retries()`
  - Lines 70-75: Buyer amount subtracts buyer_dev_fee on failure
- [x] Update Lightning payment in `src/app/release.rs::do_payment()` ⚠️ CRITICAL
  - Lines 443-448: Actual payment amount subtracts buyer_dev_fee
- [x] Add unit tests for `calculate_dev_fee()` in `src/util.rs::tests`
  - Test `test_get_dev_fee_basic`: Standard calculation (1000 @ 30% = 300)
  - Test `test_get_dev_fee_rounding`: Rounding (333 @ 30% = 100)
  - Test `test_get_dev_fee_zero`: Zero fee (0 → 0)
  - Test `test_get_dev_fee_tiny_amounts`: Tiny amounts (1 @ 30% = 0)
  - All tests passing ✓
- [x] Integration testing with various order amounts
  - Verified correct amounts in all message flows
  - Verified correct Lightning payment amounts
  - Verified no satoshi loss with odd dev_fee values

**Deliverables:** ✅ All fee calculations implemented, tested, and integrated across entire order flow

### Phase 3: Payment Execution ✅ COMPLETE

**Prerequisites:** Phase 2 complete ✅

**Completed Implementation (Commits: f508669, 102cfed, eaf3319, 42253f1, 2655943):**
- [x] Implement `send_dev_fee_payment()` in `src/app/release.rs` (lines 564-678)
  - Step 0: Dev fee amount validation (`dev_fee > 0` check)
  - Step 1: LNURL resolution with 15-second timeout
    - Call: `resolv_ln_address(DEV_FEE_LIGHTNING_ADDRESS, amount)`
    - Error handling: Log and return error on timeout/failure
  - Step 2: Create LND connector
    - Call: `LndConnector::new().await`
  - Step 3: Send payment with 5-second timeout
    - Call: `ln_client.send_payment(&payment_request, amount, tx)`
    - Error handling: Timeout prevents hanging on self-payments
  - Step 4: Wait for payment result with 25-second timeout
    - Call: Loop receiving messages until terminal status
    - Success: Return payment hash
    - Failure: Return error with details
- [x] Create scheduler job `job_process_dev_fee_payment()` in `src/scheduler.rs` (lines 511-608)
  - Uses `find_unpaid_dev_fees()` database function (lines 895-908 in `src/db.rs`)
  - Query: `SELECT * FROM orders WHERE (status = 'settled-hold-invoice' OR status = 'success') AND dev_fee > 0 AND dev_fee_paid = 0`
  - **Key improvement:** Query checks BOTH statuses to handle race conditions (commit 102cfed)
  - For each unpaid order:
    - Call `send_dev_fee_payment()` with 50-second timeout
    - On success: Update `dev_fee_paid = 1`, `dev_fee_payment_hash = hash`
    - On failure: Log error, leave `dev_fee_paid = 0` for retry
  - Schedule: Run every 60 seconds
- [x] Enhanced logging (commit 2655943)
  - Logs BEFORE/AFTER state for dev_fee_paid and dev_fee_payment_hash
  - Database update success/failure logging
  - Verification queries to confirm database persistence
  - Info: Payment initiation, success with order_id, amount, hash
  - Error: Resolution failures, payment failures, timeouts with details
- [x] Error handling and retry logic
  - Dev fee validation before payment attempt (commit eaf3319)
  - Self-payment detection (5s timeout prevents hanging)
  - LNURL resolution failures (15s timeout)
  - Routing failures (25s timeout)
  - Scheduler timeout (50s total)
  - All errors: Log and allow automatic retry on next cycle (60s)
- [x] Market price order dev_fee calculation (commit 2655943)
  - Implemented in `take_buy.rs` and `take_sell.rs`
  - Calculates `dev_fee = get_dev_fee(fee * 2)` when order amount is determined
  - Fixes bug where market price orders had `dev_fee = 0` permanently
- [x] Integration testing
  - Tested successful dev fee payment flow
  - Tested LNURL resolution failure handling
  - Tested payment timeout scenarios
  - Tested scheduler retry mechanism
  - Verified order completes regardless of dev fee payment status
  - Fixed edge cases through commits 42253f1, 102cfed, 2655943

**Deliverables:** ✅ Automated dev fee payment system fully operational with scheduler-based processing, enhanced logging, race condition handling, and automatic retry mechanism

### Phase 4: Audit Events via Nostr ⚠️ TO IMPLEMENT

**Purpose:** Provide transparent, verifiable audit trail of all dev fee payments through Nostr relays.

**What Will Be Implemented:**
- Custom Nostr event kind (38383) for dev fee payment audits
- Event publishing in scheduler after successful payment
- Complete payment details: amount, hash, order reference, timestamp
- Public relay distribution for third-party verification
- Queryable tags for analytics and reporting

**Event Specification:**

| Property | Value |
|----------|-------|
| Event Kind | 38383 (Regular Event) |
| Replaceability | No - complete audit trail |
| Published After | Successful dev fee payment & DB update |
| Content Format | JSON with structured payment data |
| Tags | `y`, `z`, `order`, `amount`, `hash`, `t`, `currency`, `network` |

**Event Kind Rationale:**

Why kind 38383 (Regular Event)?
- ✅ **Complete History:** Every payment is a separate, permanent event
- ✅ **Third-Party Auditing:** Anyone can query all historical payments
- ✅ **Total Calculation:** Sum all `amount` tags to get total dev fund contributions
- ✅ **Immutable Record:** Events cannot be replaced or deleted
- ✅ **Standard Compliance:** Follows NIP-01 application-specific event range (1000-9999)

**Event Structure Example:**

```json
{
  "kind": 38383,
  "content": {
    "order_id": "550e8400-e29b-41d4-a716-446655440000",
    "dev_fee_sats": 100,
    "payment_hash": "abc123...",
    "payment_timestamp": 1234567890,
    "destination": "dev@getalby.com",
    "order_amount_sats": 10000,
    "order_fiat_amount": 50,
    "order_fiat_code": "USD",
    "status": "success"
  },
  "tags": [
    ["y", "mostro"],
    ["z", "dev-fee-payment"],
    ["order", "550e8400-e29b-..."],
    ["amount", "100"],
    ["hash", "abc123..."],
    ["t", "audit"],
    ["t", "dev-fund"],
    ["currency", "USD"],
    ["network", "mainnet"]
  ]
}
```

**Query Examples:**

```javascript
// Get all dev fee payments
const filter = {
  kinds: [38383],
  "#y": ["mostro"],
  "#z": ["dev-fee-payment"]
};

// Calculate total dev fund contributions
let total = 0;
events.forEach(event => {
  const amountTag = event.tags.find(t => t[0] === "amount");
  if (amountTag) total += parseInt(amountTag[1]);
});

// Filter by currency
const usdPayments = {
  kinds: [38383],
  "#currency": ["USD"]
};

// Find payments for specific order
const orderPayments = {
  kinds: [38383],
  "#order": ["550e8400-e29b-41d4-a716-446655440000"]
};
```

**Implementation Details:**

**Location:** `src/scheduler.rs::job_process_dev_fee_payment()` (after payment success)

**Function:** `publish_dev_fee_audit_event(order: &Order, payment_hash: &str)`

**Error Handling:** Audit event failures are logged but don't fail the payment transaction

**Retry Logic:** None - if event publish fails, payment still succeeds (prioritize financial reliability)

**Privacy Considerations:**
- Order ID included for transparency
- Buyer/seller pubkeys NOT included (privacy)
- Only aggregate payment data published

**Benefits:**

1. **Transparency:** Anyone can verify dev fund contributions
2. **Accountability:** Public record of all fee payments
3. **Analytics:** Query by currency, date range, network
4. **Trust:** Third-party auditing without Mostro access
5. **Compliance:** Verifiable fee collection for reporting

**Testing:**

```bash
# Query dev fee events from relay
nostr-cli -k 38383 --tag y=mostro --tag z=dev-fee-payment

# Calculate total contributions
nostr-cli -k 38383 --tag y=mostro | jq '[.[] | .tags[] | select(.[0]=="amount") | .[1] | tonumber] | add'
```

**Status:** ⚠️ TO IMPLEMENT - Planned for future release

**Implementation Roadmap:**
1. Add `DEV_FEE_AUDIT_EVENT_KIND` constant to `src/config/constants.rs`
2. Create `publish_dev_fee_audit_event()` function in `src/util.rs` or new `src/app/audit_event.rs`
3. Integrate event publishing in `src/scheduler.rs` after successful payment
4. Test event publishing to relays
5. Verify third-party queryability

**Future Enhancements:**
- Aggregate statistics event (kind 38100) updated monthly with totals
- Dashboard for visualizing dev fund contributions
- NIP-05 verification for Mostro's audit event pubkey

## Monitoring and Operations

### Statistics Queries

**Unpaid Development Fees:**
```sql
-- Same query used by find_unpaid_dev_fees() in src/db.rs:895-908
SELECT id, dev_fee, created_at, status
FROM orders
WHERE (status = 'settled-hold-invoice' OR status = 'success')
  AND dev_fee > 0
  AND dev_fee_paid = 0
ORDER BY created_at DESC;

-- IMPORTANT: Query checks BOTH statuses (settled-hold-invoice AND success)
-- Reason: Handles race condition where buyer payment completes while dev fee payment is processing
-- Without both statuses: Orders could get stuck with unpaid dev fees if buyer pays before dev fee completes
-- Result: Ensures all dev fees are eventually collected regardless of payment timing
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
-- Orders with unpaid dev fees older than 5 minutes (potential issues):
-- IMPORTANT: Uses dual-status query like find_unpaid_dev_fees()
SELECT id, dev_fee, dev_fee_paid, status, failed_payment, payment_attempts, created_at
FROM orders
WHERE (status = 'settled-hold-invoice' OR status = 'success')
  AND dev_fee > 0
  AND dev_fee_paid = 0
  AND created_at < strftime('%s', 'now', '-5 minutes')
ORDER BY created_at DESC;

-- Orders with unpaid dev fees in last 24 hours:
-- Useful for monitoring recent payment failures
SELECT id, dev_fee, created_at, status, dev_fee_paid
FROM orders
WHERE (status = 'settled-hold-invoice' OR status = 'success')
  AND dev_fee > 0
  AND dev_fee_paid = 0
  AND created_at > strftime('%s', 'now', '-24 hours')
ORDER BY created_at DESC;

-- Check for orders stuck in settled-hold-invoice with unpaid dev fees:
-- These should be picked up by scheduler every 60 seconds
SELECT id, dev_fee, status, dev_fee_paid, created_at,
       (strftime('%s', 'now') - created_at) / 60 as minutes_since_creation
FROM orders
WHERE status = 'settled-hold-invoice'
  AND dev_fee > 0
  AND dev_fee_paid = 0
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

**4. Market Price Orders With Zero Dev Fee**

**Symptom:**
```sql
-- Orders with fee but no dev_fee (indicates bug in market price flow)
SELECT id, amount, fee, dev_fee, price_from_api, status
FROM orders
WHERE fee > 0
  AND dev_fee = 0
  AND price_from_api = 1;
```

**Cause:** Market price order was taken but `dev_fee` was not calculated

**Impact:** Dev fee not collected from these orders (revenue loss)

**Fix:** Ensure both `take_buy.rs` and `take_sell.rs` calculate `dev_fee` when updating `amount` and `fee` for market price orders (see Market Price Order Dev Fee Calculation section)

**Verification:**
```sql
-- All market price orders should have consistent fees
SELECT
  COUNT(*) as total_market_orders,
  SUM(CASE WHEN fee > 0 AND dev_fee = 0 THEN 1 ELSE 0 END) as broken_orders,
  SUM(CASE WHEN fee > 0 AND dev_fee > 0 THEN 1 ELSE 0 END) as correct_orders
FROM orders
WHERE price_from_api = 1
  AND amount > 0;  -- Only count taken orders
```

**5. Market Price Orders with Stale dev_fee After Timeout** ✅ FIXED

**Status:** This bug was fixed in commit `c803471`. The `update_order_to_initial_state()`
function now properly persists `dev_fee = 0` to the database.

**Historical Issue (Pre-Fix):**
The function set `dev_fee = 0` in memory but didn't include it in the SQL UPDATE statement,
causing stale values to remain in the database. When `edit_pubkeys_order()` fetched the
order from the database, it would return the old dev_fee value.

**Symptom (Before Fix):**
```sql
-- Orders that timed out but dev_fee wasn't reset in database
SELECT id, amount, fee, dev_fee, price_from_api, status
FROM orders
WHERE status = 'pending'
  AND price_from_api = 1
  AND amount = 0
  AND fee = 0
  AND dev_fee != 0;  -- BUG: Should be 0
```

**Cause:** `update_order_to_initial_state()` didn't persist dev_fee to database

**Impact:** Next taker would be charged incorrect dev_fee from previous attempt

**Fix Applied:** Added `dev_fee` parameter to `update_order_to_initial_state()` and
included it in the SQL UPDATE statement:
- `src/db.rs` lines 776-817: Function signature and SQL UPDATE modified
- `src/app/cancel.rs` lines 18-25: Explicit cancellation handler

**Prevention:** Both paths now include `order.dev_fee = 0` for market price orders. See "Taker Abandonment and Order Reset" section for details.

**Verification:**
```sql
-- All pending market price orders should have dev_fee = 0
SELECT COUNT(*) as stale_dev_fee_orders
FROM orders
WHERE status = 'pending'
  AND price_from_api = 1
  AND amount = 0
  AND fee = 0
  AND dev_fee != 0;
-- Should return 0 if fix is working correctly
```

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

## Testing Specification

### Unit Tests

**Status:** ✅ IMPLEMENTED

**Location:** `src/util.rs::tests` module (lines 1453-1478)

**Tests Implemented:**

1. **`test_get_dev_fee_basic`** (lines 1453-1458)
   - Purpose: Standard percentage calculation
   - Test: 1,000 sats @ 30% = 300 sats
   - Status: ✓ Passing

2. **`test_get_dev_fee_rounding`** (lines 1460-1465)
   - Purpose: Rounding behavior
   - Test: 333 sats @ 30% = 99.9 → rounds to 100 sats
   - Status: ✓ Passing

3. **`test_get_dev_fee_zero`** (lines 1467-1471)
   - Purpose: Zero fee handling
   - Test: 0 sats @ 30% = 0 sats
   - Status: ✓ Passing

4. **`test_get_dev_fee_tiny_amounts`** (lines 1473-1478)
   - Purpose: Small amount edge cases
   - Test: 1 sat @ 30% = 0.3 → rounds to 0 sats
   - Status: ✓ Passing

**All tests use `calculate_dev_fee()` directly with explicit percentage (0.30) to avoid dependency on global Settings.**

**Run tests:**
```bash
cargo test test_get_dev_fee
# Output: test result: ok. 4 passed; 0 failed
```

**Test Coverage:**
- ✅ Standard calculations
- ✅ Rounding behavior (both up and down)
- ✅ Zero fee edge case
- ✅ Tiny amounts (rounds to zero)
- ✅ All tests passing in CI

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

