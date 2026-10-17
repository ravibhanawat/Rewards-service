# Engineering decisions

This document explains the rules this service has to keep true, where each rule is enforced, and the trade-offs I made to get there. Code references are relative to the repository root. The API reference and error-code table are in `README.md`.

The short version:

- **Checkout runs in one PostgreSQL transaction.** It uses READ COMMITTED isolation with explicit row locks, taken in a fixed order: cart, then coupon, then products by id. A checkout either applies all of its effects or none of them.
- **The order row doubles as the idempotency record.** It is written in the same transaction as the inventory decrement, so "this key was used" and "this order exists" are one fact.
- **Coupons have no mutable status.** A coupon is redeemed exactly when an order references it, and `orders.coupon_id` is `UNIQUE`.
- **Milestones are counted, not sequenced.** They come from the number of committed orders, read while holding a database-wide advisory lock.
- **Constraints back up the code.** Every invariant the database can express is also a constraint or trigger, so a bug in application code fails loudly instead of corrupting data.

---

## 1. System invariants

Each invariant lists where it is enforced in application code, the database backstop, and the test that would fail if it broke.

| # | Invariant | Application enforcement | Database backstop | Proven by |
|---|---|---|---|---|
| I1 | **Inventory is never oversold and never negative.** | `checkout` in `src/services/checkout.rs` locks product rows (`FOR NO KEY UPDATE OF p`, ordered by id) and validates stock while holding the locks. The decrement is also guarded (`AND p.stock >= u.quantity`, and the affected-row count is checked). | `products_stock_non_negative CHECK (stock >= 0)` | `concurrency.rs::concurrent_checkouts_never_oversell_limited_stock`, `overlapping_carts_do_not_deadlock_or_oversell` |
| I2 | **A cart produces at most one order.** | The cart row lock (`SELECT status … FOR UPDATE`) plus the `status = 'open'` check in `checkout`. | `orders_cart_unique UNIQUE (cart_id)`, mapped to `CART_ALREADY_CHECKED_OUT` | `checkout.rs::a_cart_is_checked_out_at_most_once_even_with_a_new_key`, `concurrency.rs::concurrent_checkouts_of_one_cart_with_different_keys_create_one_order` |
| I3 | **A retried checkout never creates a second order or decrements stock twice.** | `order_for_key` and `replay` in `checkout.rs`, plus the replay path after the cart lock. | `orders_idempotency_key_unique` | `checkout.rs::retrying_with_the_same_key_replays_the_order_without_side_effects`, `concurrency.rs::concurrent_retries_of_one_checkout_create_exactly_one_order` |
| I4 | **An idempotency key is bound to exactly one request.** | The request fingerprint is compared in `replay`. | `orders_idempotency_key_unique`, mapped to `IDEMPOTENCY_KEY_REUSED` | `checkout.rs::reusing_a_key_for_a_different_request_is_rejected` |
| I5 | **A coupon is redeemed at most once.** | `lock_unredeemed_coupon` (`FOR UPDATE` on the coupon row, then an `EXISTS` check on orders). | `orders_coupon_unique UNIQUE (coupon_id)` | `concurrency.rs::a_coupon_is_redeemed_by_at_most_one_of_many_concurrent_checkouts`, `coupons.rs::discount_rounds_half_up_and_a_coupon_is_single_use` |
| I6 | **A failed checkout consumes nothing: no coupon, no inventory, and the cart is not closed.** | Every effect lives in one transaction, and validation runs before any write. | Atomic commit/rollback. Coupon state is derived from orders, so there is nothing separate to "un-claim". | `checkout.rs::failed_checkout_leaves_coupon_inventory_and_cart_untouched`, `concurrency.rs::checkouts_that_fail_while_holding_the_coupon_do_not_consume_it`, `checkout.rs::invalid_checkout_requests_fail_without_side_effects` |
| I7 | **An order explains its own total and never changes afterwards.** | `order_items` snapshots the name, unit price, quantity and line total. `orders` snapshots the coupon code and percent. | `order_items_line_total_reconciles`, `orders_total_reconciles`, `orders_discount_within_subtotal`, `orders_coupon_snapshot_complete`. Triggers `orders_immutable` and `order_items_immutable` reject UPDATE and DELETE. | `checkout.rs::checkout_creates_an_order_whose_snapshot_survives_product_changes` (it also tries to tamper through SQL and expects an error) |
| I8 | **Money totals are exact and never negative.** | `src/money.rs`: integer paise, checked arithmetic, one rounding per order, and the discount is capped at the subtotal. | `CHECK`s on `subtotal_paise`, `discount_paise` and `total_paise >= 0`; `discount_paise <= subtotal_paise` | Unit tests in `src/money.rs`; `coupons.rs::a_full_discount_yields_a_zero_total_never_a_negative_one` |
| I9 | **Each order milestone yields at most one coupon, and only once it has been reached.** | `coupons::generate` in `src/services/coupons.rs` holds a `pg_advisory_xact_lock` and counts committed orders. | `coupons_milestone_unique`; the `coupons_immutable` trigger | `coupons.rs::coupon_becomes_available_exactly_at_the_milestone`, `delayed_generation_rewards_each_missed_milestone_once_in_order`, `failed_checkouts_do_not_count_towards_milestones`, `concurrency.rs::concurrent_coupon_generation_never_duplicates_a_milestone` |
| I10 | **A cart's contents cannot change during or after checkout.** | Cart mutations call `lock_open_cart` (`UPDATE carts … WHERE status = 'open'`), which conflicts with checkout's cart lock. | `carts.status CHECK` | `concurrency.rs::items_added_during_checkout_are_either_ordered_or_rejected` |
| I11 | **Invalid lines never enter a cart.** | `validate_quantity` and `check_purchasable` in `src/services/carts.rs` | `cart_items` primary key `(cart_id, product_id)`, `CHECK (quantity BETWEEN 1 AND 1000)`, FK to `products` | `carts.rs::invalid_products_and_quantities_never_enter_the_cart` |
| I12 | **Reports reconcile with the orders and coupons the API returns, and reading a report changes nothing.** | `reports::summary` reads one REPEATABLE READ, READ ONLY snapshot with three separate aggregates. | `READ ONLY` transaction | `reporting.rs::report_reconciles_with_orders_and_coupons_and_never_mutates_state` |

---

## 2. Ambiguities and the semantics I chose

**Price changes between "add to cart" and checkout.** Cart prices are indicative. `GET /carts/{id}` always shows *current* prices, and checkout charges the price it reads while holding the product row lock. The order stores that price, so what was charged is always visible. To avoid charging an amount the customer never saw, checkout accepts an optional `expected_subtotal_paise`. If it is present and differs from the locked subtotal, checkout fails with `409 PRICE_CHANGED` (details: expected and actual subtotal) and changes nothing. I made the guard opt-in so the stated policy (proceed at the new price) still holds for clients that don't send it.

**Availability changes.**
- Add and update run an *advisory* stock check, so a line for 6 units of a product with 5 in stock never enters the cart. This check is **not a reservation**.
- Checkout re-validates everything under row locks.
- The cart view flags lines that can't currently be bought with `purchasable: false` and shows `stock_available`.
- A product that "no longer exists" is modelled as `active = false` (discontinued), not deleted. Historical `order_items` keep a valid foreign key, and checkout fails with `409 PRODUCT_UNAVAILABLE`, listing every affected product id.

**What a "successfully placed order" is.** It is a committed row in `orders`, nothing more. Failed or rolled-back attempts leave no row and never count towards milestones (`coupons.rs::failed_checkouts_do_not_count_towards_milestones`).

**How milestones are calculated.** Milestone *k* is reached when at least `k × n` orders have committed. The count is taken when the administrator requests generation, not when the order commits. Coupons store `milestone_order_count` (n, 2n, 3n, …) rather than *k*.

**Several eligible milestones, and delayed generation.** Generation is admin-triggered, so milestones can pile up. Each call generates **one** coupon, for the **earliest** reached milestone that has no coupon yet. The response includes `remaining_eligible_milestones` so the administrator knows to call again. When nothing is eligible, the response is `409 NO_ELIGIBLE_MILESTONE` with `successful_orders`, `every_n_orders`, `next_milestone_order_count` and `orders_until_next_milestone`. I chose one coupon per call so that every call has a single, simple effect.

**Coupon scope.**
- **Global bearer codes.** Anyone holding a code can redeem it once, because there are no customer accounts to scope it to. For the same reason, codes are random (`RWD-` followed by 10 hex characters, about 40 random bits) rather than derived from the milestone.
- **One coupon per checkout, no stacking.**
- **What it applies to.** The discount applies to the whole order subtotal. There is no minimum order value and no expiry.
- **Code matching.** Codes are matched case-insensitively after trimming whitespace.
- **Unknown versus used.** An unknown code returns `422 UNKNOWN_COUPON`; a code that has already been used returns `409 COUPON_ALREADY_REDEEMED`.

**Zero-value orders.** Allowed: prices may be 0 and a 100 % coupon gives a total of 0. Totals can never be negative; see §5.

**Rounding.** One rounding step per order: the percentage discount on the subtotal, rounded half up to the nearest paisa. Details are in §5.

**Can a failed idempotent request be retried with the same key?** Yes. A key is bound only when it *produces an order*. A failed attempt has no side effects, so nothing needs protecting, and the client can fix the problem (remove the sold-out line, drop the coupon) and retry with the same key (`checkout.rs::invalid_checkout_requests_fail_without_side_effects`, `failed_checkout_leaves_coupon_inventory_and_cart_untouched`). The cost is that one key can see a failure first and a success later. I think that's correct: the key promises "at most one order", not "the same response forever".

**Same key, different cart or payload.** Returns `422 IDEMPOTENCY_KEY_REUSED`. Keys are global because there are no accounts to scope them to.

**Adding a product that is already in the cart.** Returns `409 ITEM_ALREADY_IN_CART` rather than incrementing the quantity. An incrementing POST is not retry-safe: a timed-out "add 1" that the client retries becomes "add 2". `PATCH /carts/{id}/items/{product_id}` sets an *absolute* quantity, so it is naturally idempotent. A PATCH to quantity 0 returns 422; removal is an explicit `DELETE`.

**Admin stock edits are relative.** `PATCH /admin/products/{id}` takes `stock_delta`, not an absolute stock value. Here is the problem with absolute values: an admin reads stock 10, three checkouts commit, then the admin writes "15" meaning "+5". The result is 15 instead of 12, and the checkouts' decrements are lost. A delta is applied atomically as `stock = stock + $delta` with a `stock + $delta >= 0` guard.

**`n` and `x`.** These are startup configuration (`REWARD_EVERY_N_ORDERS`, `REWARD_DISCOUNT_PERCENT`), validated at boot (`RewardPolicy::new`: `n >= 1`, `1 <= x <= 100`).
- Changing `x` only affects coupons generated later, because each coupon stores its own percent.
- Changing `n` is **not safe** today. Moving from n=5 to n=3 makes milestones 3, 6, 9 look unrewarded even though coupons already exist for 5 and 10. I document this rather than solve it; see §11.

---

## 3. Material design decisions

## Decision: One database transaction per checkout, READ COMMITTED with pessimistic row locks

**Context:** Checkout touches four kinds of state: inventory, order, coupon and cart. A partial outcome is a correctness bug: stock decremented without an order, a coupon consumed by a failed order, a cart closed with no order. The writes also have to stay correct under concurrent checkouts of the same products and the same coupon.

**Options considered:**
1. A single transaction at **SERIALIZABLE**, retrying on `40001`.
2. A single transaction at **READ COMMITTED**, with explicit row locks (`SELECT … FOR UPDATE`) taken before validating.
3. **Optimistic conditional updates** with no read locks: `UPDATE products SET stock = stock - q WHERE id = $1 AND stock >= q`, then check the affected-row count line by line.
4. Multi-step workflows or sagas, such as reserving inventory first and confirming later.

**Choice:** Option 2. The conditional-update guard from option 3 and the `CHECK (stock >= 0)` constraint stay in place as defence in depth.

**Why:**
- **SERIALIZABLE** would be correct, but every conflict becomes a whole-transaction abort that the server or client must retry. Under a hot product, aborts are the *normal* outcome, so they need a bounded retry loop and the error model becomes harder to explain.
- **Pessimistic locks queue contenders instead of aborting them.** Each waiter then gets a precise answer: success, or `INSUFFICIENT_INVENTORY` with per-line `requested`/`available`. Both results are deterministic and testable.
- **Locking all products first lets me validate the whole cart before writing.** I can report *every* short line at once, and the prices I charge are the prices I validated, because no one can reprice a row while I hold it.
- **Option 3 on its own** would discover shortfalls one line at a time, mid-write. It also says nothing about price stability.
- **Option 4** only pays for itself once there is a non-transactional participant, such as a payment provider (see the payment decision).

**Consequences:**
- Checkouts of the same product run strictly one after another for the duration of the transaction. The transaction is short (about ten small statements, no network calls outside Postgres), so this is fine at this scale. In production, a single hot SKU becomes the limit; see §8.
- Waiting is bounded by `lock_timeout = 5s` (set in `src/db.rs`). After that the client gets `409 CONCURRENT_UPDATE` with `retryable: true` rather than a hung request.

## Decision: A fixed global lock order (cart → coupon → products by id) and the weakest sufficient lock modes

**Context:** Two checkouts with overlapping products would deadlock if each locked rows in its own cart's insertion order. Cart mutations racing a checkout could also slip an item into a cart after its contents had been read.

**Options considered:** Lock in cart order and rely on Postgres deadlock detection plus a retry. Use a single global checkout mutex. Use a fixed total order over all lockable rows.

**Choice:** Every checkout takes locks in the same order:
1. The **cart** row with `FOR UPDATE`.
2. The **coupon** row, if any, with `FOR UPDATE`.
3. **Product** rows with `FOR NO KEY UPDATE OF p … ORDER BY p.id`. Postgres applies `ORDER BY` below the lock step, so rows are locked in id order.

Cart mutations take the cart row lock through `lock_open_cart`, an `UPDATE carts … WHERE id = $1 AND status = 'open'`.

**Why:**
- **Products use `FOR NO KEY UPDATE`, not `FOR UPDATE`.** Adding a product to *any* cart runs a foreign-key check that takes `FOR KEY SHARE` on the product row. `FOR UPDATE` conflicts with `KEY SHARE`; `FOR NO KEY UPDATE` does not. So holding a product inside a checkout doesn't block other customers from adding that product to their carts. We never change a product's key, so the weaker mode is enough. It's also exactly the lock the later `UPDATE products` needs anyway.
- **The coupon is locked before the products.** Coupon contention is rare, but its losers are certain: of N checkouts using one code, N−1 must fail. Taking the coupon first means a losing redeemer waits and fails without ever holding hot product rows that would block unrelated checkouts.
- **The cart lock comes first** because it serializes everything about one cart: duplicate retries, different-key double submits, and item edits.

**Consequences:** No cycle is possible among checkouts; the full analysis is in §4. The guarantee depends on discipline: any new code path that locks more than one of these rows must follow the same order. `overlapping_carts_do_not_deadlock_or_oversell` adds the same two products to carts in opposite orders. A deadlock would surface as `CONCURRENT_UPDATE` and fail the test.

## Decision: The order row is the idempotency record, compared by a SHA-256 request fingerprint

**Context:** Clients retry a checkout when they time out. The retry must not create a second order. It should also return the *original* result, so the client learns that its first attempt succeeded.

**Options considered:**
1. An **in-memory key map.** It is lost on restart and not shared between instances, so I rejected it outright.
2. A **separate `idempotency_keys` table** with `in_progress` / `completed` states and a stored response. This is the Stripe and Brandur Leach "idempotency keys with recovery points" design.
3. **Store the key and a request fingerprint on the order row**, inside the checkout transaction.
4. **Use `cart_id` alone as the natural key**, since a cart can only be checked out once.

**Choice:** Option 3.
- The `Idempotency-Key` header is required: 1–255 visible ASCII characters.
- `orders.idempotency_key` is `UNIQUE` (`orders_idempotency_key_unique`).
- `orders.request_fingerprint` is the SHA-256 of a canonical string: `checkout/v1|cart=…|coupon=<normalized code>|expected_subtotal=…`.
- A replay returns `200` with the same body and the header `Idempotent-Replayed: true`. A new order returns `201` with a `Location` header.

**Why:**
- **The key and the effect commit atomically.** "The key was used" and "the order exists" are one row. There is no window where the stock is decremented but the key is unrecorded, or the reverse. Option 2 only earns its extra state machine, recovery logic and stored responses when part of the work happens *outside* the database transaction, such as a payment API call.
- **`cart_id` alone (option 4) isn't enough.** A retry after a lost response would get `409 CART_ALREADY_CHECKED_OUT`, which looks exactly like "someone else, in another tab, checked this cart out". The client can't tell its own success from a conflict. The key also lets us detect key reuse with a different payload.
- **Status codes follow the IETF draft** `draft-ietf-httpapi-idempotency-key-header`: missing key → 400, reused with a different payload → 422. The draft suggests 409 for "the original is still in flight". In that case we *wait* on the cart lock instead, then replay, which is friendlier. If the wait exceeds `lock_timeout`, the client gets `409 CONCURRENT_UPDATE`.

**Consequences:**
- Keys never expire (no TTL) and are global, not per customer; see §7.
- Failed attempts aren't recorded (see §2), which is deliberate.
- **Same key on two different carts at the same instant.** Both requests pass the lookup; the second blocks on the unique index until the first commits, then hits `orders_idempotency_key_unique`. That violation is mapped to `422 IDEMPOTENCY_KEY_REUSED`, not a 500.

## Decision: Coupon redemption is derived from orders, and coupons and orders are append-only

**Context:** The obvious schema gives coupons a mutable `status` (`available`/`redeemed`) and a `redeemed_order_id`, written during checkout. That creates two sources of truth that must be kept in sync under concurrency and rollback.

**Options considered:**
1. A mutable status column on `coupons`, updated in the checkout transaction.
2. A conditional claim (`UPDATE coupons SET status='redeemed' WHERE id=$1 AND status='available'`).
3. No status column: a coupon is redeemed **if and only if** an order references it, with `orders.coupon_id UNIQUE`.

**Choice:** Option 3.
- `CouponView.status` and the report's coupon counts are computed with `LEFT JOIN orders o ON o.coupon_id = c.id`.
- Checkout serializes redeemers by locking the coupon row (`FOR UPDATE`, used purely as a mutex), then checks `EXISTS (SELECT 1 FROM orders WHERE coupon_id = $1)`.
- `orders_coupon_unique` is the hard backstop. A violation maps to `COUPON_ALREADY_REDEEMED`.
- `coupons`, `orders` and `order_items` reject UPDATE and DELETE through triggers (`coupons_immutable`, `orders_immutable`, `order_items_immutable`).

**Why:**
- **A coupon can't be "lost" by a failed checkout.** Nothing was written outside the rolled-back transaction, so there is nothing to compensate.
- **Coupon state can't disagree with orders,** because it *is* the orders.
- **Reports reconcile by construction:** redeemed = the number of orders with a coupon.
- **Append-only facts make every derived number reproducible.** The triggers turn a would-be silent history rewrite into an error. The snapshot test tries one through raw SQL and expects it to fail.

**Consequences:**
- Adding "void" or "expired" states later needs a column or a separate events table.
- Reading coupon status costs a join, which is cheap because `orders_coupon_unique` is an index.

## Decision: Milestones from `count(orders)` under an advisory lock, not from a sequence

**Context:** "Every nth successful order makes a coupon available" needs a reliable count of *successful* orders, and concurrent administrators must not generate a coupon for the same milestone twice.

**Options considered:**
1. **A Postgres `SEQUENCE` / `order_number` column**, with milestones at `order_number % n == 0`.
2. **A gapless counter row** (`UPDATE counters SET value = value + 1 RETURNING value`) inside checkout.
3. **Count committed orders at generation time.** Serialize generators with `pg_advisory_xact_lock`, with `UNIQUE (milestone_order_count)` as a backstop.

**Choice:** Option 3, in `coupons::generate`.
- Generators queue on `pg_advisory_xact_lock(0x636f_7570_6f6e)` (ASCII "coupon").
- The function counts orders, then finds the smallest `m` in `generate_series(n, count, n)` that has no coupon, and inserts it.

**Why:**
- **Sequences are non-transactional.** `nextval` isn't rolled back, so every failed checkout leaves a gap and order numbers stop meaning "the k-th successful order". Milestones would be skipped or misassigned. This is a real trap: the prompt I worked from suggested "a stable order sequence", and I redirected it.
- **A gapless counter row** is transactional, but it is one hot row that every checkout in the system would lock until commit. That serializes all checkouts globally, just to support an admin-triggered feature.
- **Counting is exact and cheap enough.** Orders are append-only and exist only once committed, so `count(*)` is monotonic and equals "successful orders".
- **The advisory lock is database-wide,** so the protection holds across any number of app instances. Because the transaction runs at READ COMMITTED, the second generator's count starts *after* the first commits, so it sees the new coupon and moves on to the next milestone (or reports none). The unique constraint means even a bypass of the lock can't create a duplicate. A violation maps to `CONCURRENT_UPDATE`.

**Consequences:**
- We don't record *which* order crossed the milestone; we only know the threshold. That's sufficient because coupons are global (not owed to that customer).
- `count(*)` is O(orders). At scale it would become a maintained counter updated by a trigger or job, or a periodic job; see §8.

## Decision: Integer paise, checked arithmetic, one half-up rounding on the subtotal

**Context:** Money must be exact, deterministic and never negative, and orders must reconcile line by line.

**Options considered:** `f64` (rejected: binary fractions). A decimal type such as `rust_decimal` or Postgres `NUMERIC`. Integer minor units. For rounding: per-line versus per-order; half-up versus banker's rounding versus floor.

**Choice:** `i64` paise everywhere (`BIGINT` in Postgres, `_paise` suffix in the JSON). `src/money.rs`:
- `line_total` uses `checked_mul`; `price_order` uses `checked_add`. Overflow becomes `422 VALIDATION_FAILED` (field `cart`), never a wrap-around.
- `percentage_discount(subtotal, pct)` computes `(subtotal × pct + 50) / 100` in `i128`, then takes `min(subtotal)`.

**Why:**
- **Integers make equality exact,** so the database can enforce `line_total_paise = unit_price_paise * quantity` and `total_paise = subtotal_paise - discount_paise` as `CHECK`s.
- **Discounting the subtotal once** means one rounding per order and no per-line drift. The report sums stored integers, so it can never be off by accumulated rounding.
- **Half-up is the rule most people expect.** Ties go to the customer, by at most half a paisa.
- **`i128` intermediates** mean `i64::MAX × 100` can't overflow (unit-tested).

**Consequences:** The service is single-currency (INR). Clients format amounts for display themselves. Admin price edits are capped at `MAX_UNIT_PRICE_PAISE = 100_000_000_000` (₹1,000,000,000) and line quantity at 1,000, so realistic carts sit nowhere near the overflow path, but it is still handled.

## Decision: One typed error enum, one HTTP mapping, codes over status codes

**Context:** API clients need to tell apart "fix your request", "the world changed, maybe retry" and "not found", and they need machine-readable details such as which product is short and by how much.

**Options considered:** Ad-hoc `(StatusCode, String)` per handler. A generic `{status, message}`. A typed enum with a stable `code` and structured `details`.

**Choice:**
- `AppError` in `src/error.rs` is one variant per failure. `status_and_code()` is the single place that maps a variant to an HTTP status and a code; `details()` builds structured context.
- The body is always `{"error": {"code", "message", "details?"}}`.
- Axum's own rejections are re-wrapped by the `Json`/`Path` extractors in `src/extract.rs`, so malformed input also returns the envelope.

The status rule I applied:

| Status | Meaning | Examples |
|---|---|---|
| **400** | Malformed: unparseable JSON, wrong types, unknown fields, bad UUID in the path, missing or invalid `Idempotency-Key`. | `INVALID_REQUEST`, `IDEMPOTENCY_KEY_MISSING` |
| **404** | The resource named in the **URL** doesn't exist. | `CART_NOT_FOUND`, `ITEM_NOT_IN_CART`, `ROUTE_NOT_FOUND` |
| **409** | Well-formed, but conflicts with **current server state**; it could succeed if that state changed. | `CART_ALREADY_CHECKED_OUT`, `EMPTY_CART`, `ITEM_ALREADY_IN_CART`, `INSUFFICIENT_INVENTORY`, `PRODUCT_UNAVAILABLE`, `COUPON_ALREADY_REDEEMED`, `PRICE_CHANGED`, `NO_ELIGIBLE_MILESTONE`, `CONCURRENT_UPDATE` |
| **422** | Well-formed, but the **content is invalid on its own**. | Quantity out of range, `UNKNOWN_PRODUCT` / `UNKNOWN_COUPON` referenced from the body, `IDEMPOTENCY_KEY_REUSED` |
| **503** | Connection pool exhausted. | `SERVICE_UNAVAILABLE` (retryable) |
| **500** | Anything unexpected. The cause is logged with `tracing::error!`; the body is generic and never contains SQL, constraint names or internals. | `INTERNAL_ERROR` |

**Why:**
- **Clients switch on `code`;** the status is only the category.
- **`#[serde(deny_unknown_fields)]` on every request body** turns typos into loud 400s. For example, `{"coupon": "SAVE"}` would otherwise silently check out *without* the discount.
- **`quantity` is parsed as `i64`** so that `-3` reaches domain validation (422) instead of being a type error (400). `1.5` and `"2"` are still 400s.

**Consequences:**
- Adding a failure mode means adding a variant, and the compiler forces a status mapping for it.
- Unknown routes and wrong methods also use the envelope (`ROUTE_NOT_FOUND`, `METHOD_NOT_ALLOWED`) through Axum's `fallback` and `method_not_allowed_fallback`.
- `EMPTY_CART` as 409 rather than 422 is a judgement call: the cart's contents are server state.

## Decision: No payment fake; a committed checkout *is* payment success

**Context:** The brief allows treating checkout success as payment success, or adding a payment abstraction.

**Options considered:** A `PaymentGateway` trait with an always-succeeding fake. A fake that fails randomly or on demand. No payment step.

**Choice:** No payment step. Committing the checkout transaction is the payment.

**Why:**
- **A fake that can't fail independently adds a seam with nothing behind it.** It also hides the real design change a payment provider forces.
- **A real provider is a non-transactional side effect** that can succeed while our commit fails, or the reverse. That changes the architecture rather than adding a call:
  1. Commit a `pending` order with an inventory reservation (with a TTL) and a payment-intent idempotency key, and commit it.
  2. Call the provider *outside* the transaction, using the provider's own idempotency key.
  3. Confirm (`pending → placed`, which is when it counts towards milestones) or compensate (release the reservation and the coupon claim).
  4. Reconcile stragglers with a background job driven by webhooks or polling.
  5. Publish events through an outbox.

  At that point the separate idempotency table with recovery points (the alternative to the order-row idempotency record) becomes necessary.

**Consequences:** The single-transaction guarantees in this repo are honest *for this scope*. §8 describes what changes with a real provider.

## Decision: Services own their SQL; no repository layer

**Context:** The suggested structure had models, handlers, services and repositories.

**Choice:**
- `src/routes.rs`: HTTP mapping only.
- `src/services/*.rs`: one module per area. Each public function owns its transaction boundary and contains its SQL.
- `src/money.rs`: pure arithmetic.
- `src/models.rs`: response shapes.
- Queries use runtime `sqlx::query_as`, so building doesn't need a live database.

**Why:**
- **The locking clauses *are* the concurrency design.** Hiding `FOR NO KEY UPDATE … ORDER BY p.id` behind a `ProductRepository::lock_many()` would put the most important line in the codebase two files away from the code that relies on it. A repository layer here would mostly be pass-through.

**Consequences:**
- Unit-testing services without Postgres isn't possible. That's acceptable, because the properties worth testing (locking, constraints, isolation) only exist in Postgres. Pure logic (money) *is* unit-tested.
- If a second storage backend ever appeared, I would extract interfaces then.

---

## 4. Transaction, concurrency and idempotency strategy

### The checkout transaction (`checkout` in `src/services/checkout.rs`)

**Before the transaction:**
- The handler (`routes::checkout`) validates the `Idempotency-Key` and parses the optional body.
- The service normalizes the coupon code (trim, uppercase, 1–64 characters) and computes the request fingerprint.

**Inside one READ COMMITTED transaction:**

1. **Key lookup** (`order_for_key`). If an order already carries this key, either replay it (fingerprint matches → `200`) or reject (`422 IDEMPOTENCY_KEY_REUSED`). No lock is needed, because a found order is committed and immutable.
2. **Cart lock**: `SELECT status FROM carts WHERE id = $1 FOR UPDATE`.
   - Missing cart → `404`.
   - Status `checked_out` → look up the cart's order (`order_for_cart`). If that order carries *our* key (we lost a race against our own duplicate), replay it. Otherwise → `409 CART_ALREADY_CHECKED_OUT`.
3. **Coupon lock**, if a code was given (`lock_unredeemed_coupon`):
   - `SELECT … FROM coupons WHERE code = $1 FOR UPDATE`. Missing → `422 UNKNOWN_COUPON`.
   - Then `EXISTS (SELECT 1 FROM orders WHERE coupon_id = $1)`. True → `409 COUPON_ALREADY_REDEEMED`.
4. **Product locks**: `SELECT … FROM cart_items JOIN products … ORDER BY p.id FOR NO KEY UPDATE OF p`. Then validate, in this order, before any write:
   - no lines → `409 EMPTY_CART`;
   - any inactive product → `409 PRODUCT_UNAVAILABLE` (lists all of them);
   - any shortfall → `409 INSUFFICIENT_INVENTORY` (lists every line with `requested`/`available`);
   - price the order (`money::price_order`);
   - the optional guard → `409 PRICE_CHANGED`.
5. **Effects:**
   - Decrement stock: one `UPDATE … FROM unnest(...) … WHERE p.stock >= u.quantity`. The affected-row count must equal the number of lines.
   - `INSERT` the order. `orders_idempotency_key_unique`, `orders_coupon_unique` and `orders_cart_unique` violations are mapped to their domain errors.
   - `INSERT` the order items (one statement over `unnest`).
   - Mark the cart `checked_out`.
   - Load the order view, then `COMMIT`.

Any error at any step returns early. Dropping the uncommitted sqlx transaction rolls it back, so there are no partial effects.

### Locks held, by operation

| Operation | Locks, in order | Lock modes |
|---|---|---|
| Checkout | cart → coupon (optional) → products ascending by id | `FOR UPDATE` → `FOR UPDATE` → `FOR NO KEY UPDATE` |
| Add, update or remove a cart item | cart (through `UPDATE carts … WHERE status='open'`); the FK check on `cart_items` insert takes `KEY SHARE` on the product | `NO KEY UPDATE` on the cart; `KEY SHARE` on the product |
| Admin product update | a single product (`UPDATE … WHERE id = $1 AND stock + $4 >= 0`) | `NO KEY UPDATE` |
| Coupon generation | `pg_advisory_xact_lock(GENERATION_LOCK_KEY)`, then an insert | advisory |
| Report, order list | none (snapshot read) | REPEATABLE READ, READ ONLY |

### Isolation assumptions

- **A waiting lock returns the latest row.** Under READ COMMITTED, a `SELECT … FOR UPDATE` that waited for another transaction returns the *latest committed version* of the row. That is how a second checkout of the same cart sees `checked_out`.
- **Each statement takes a fresh snapshot.** So the coupon `EXISTS` check, issued *after* the coupon lock was granted, sees the order committed by the previous lock holder. If that holder rolled back, it sees nothing and proceeds. This is why the coupon check is a separate statement after the lock and not part of the locking query.
- **A blocked conditional update re-checks its condition.** `lock_open_cart`'s `UPDATE … WHERE status = 'open'` re-evaluates its `WHERE` against the new row version after blocking, so an add that waited behind a checkout correctly finds the cart closed.
- **These semantics fit; higher levels would hurt.** Under REPEATABLE READ, a write to a row changed concurrently raises a serialization failure instead. That would turn ordinary contention into aborts.
- **Reports and `GET /admin/orders` use REPEATABLE READ, READ ONLY,** so their several queries share one snapshot. Order totals, per-product sums and coupon counts reconcile even while checkouts commit, and the transaction can't write.

### Why there are no deadlocks

- **Checkout against checkout.** Two checkouts of the *same* cart conflict at step 2, while holding nothing. Different carts acquire the coupon, then products, in one global order, so no wait cycle can form.
- **Waiting on a unique index.** A checkout can wait on another transaction's uncommitted unique-index entry: the same idempotency key on a different cart, or the same coupon if someone bypassed the coupon lock. The transaction it waits for performs only writes to rows it already locked, so the wait can't close a cycle.
- **Cart mutations** hold only their own cart's lock. Their product FK check takes `KEY SHARE`, which doesn't conflict with checkout's `NO KEY UPDATE`, so they never wait on a product.
- **Admin product updates** hold one lock and wait for at most one.
- **Coupon generation** takes no row locks on existing rows.
- **Backstops.** If a future change breaks the ordering, Postgres detects the deadlock (`40P01`) and the transaction rolls back cleanly. `lock_timeout = 5s` and `statement_timeout = 15s` (set as connection startup options in `src/db.rs`) bound every wait. `40001`, `40P01` and `55P03` all map to `409 CONCURRENT_UPDATE` with `retryable: true`. Pool exhaustion (`PoolTimedOut`, after a 5 s acquire timeout) maps to `503`.

### Behaviour for each scenario

| Scenario | Outcome |
|---|---|
| Same key, same payload, **sequential** | The first request gets `201`. Every retry gets `200`, `Idempotent-Replayed: true`, the identical body, and has no effect. |
| Same key, same payload, **concurrent** | One request wins the cart lock and commits `201`. The others wait, see `checked_out`, find an order carrying their key with a matching fingerprint, and return `200` replays. Exactly one order exists and stock is decremented once (`concurrent_retries_of_one_checkout_create_exactly_one_order`). |
| Same key, different payload (coupon or price guard) | `422 IDEMPOTENCY_KEY_REUSED`, no effects. |
| Same key, different cart, **sequential** | Step 1 finds the order with a different fingerprint → `422`. The other cart stays open. |
| Same key, different cart, **concurrent** | Both pass step 1. The loser blocks on `orders_idempotency_key_unique`, gets a unique violation once the winner commits, and returns `422`. Its transaction rolls back. |
| Different keys, same cart | One `201`. The rest return `409 CART_ALREADY_CHECKED_OUT`, sequentially or concurrently. |
| Client times out **after commit, before the response** | Retrying with the same key returns the original order (`200`). Retrying with a *new* key returns `409 CART_ALREADY_CHECKED_OUT`, and `GET /carts/{id}` exposes `order_id` so the client can recover. If the database connection drops *during* `COMMIT`, the application itself can't know whether the commit landed (it returns a 500); a same-key retry resolves it either way, because it either replays the committed order or performs the checkout once. |
| Insufficient inventory | `409 INSUFFICIENT_INVENTORY` with every short line. Nothing is decremented (including the lines that had enough), the coupon is untouched, and the cart stays open for editing. |
| Unknown coupon / already-redeemed coupon | `422 UNKNOWN_COUPON` / `409 COUPON_ALREADY_REDEEMED`, no effects. |
| Concurrent redeemers of one coupon | Exactly one succeeds. The rest return `409 COUPON_ALREADY_REDEEMED`, and their carts and stock are untouched. |
| Redeemers that fail while holding the coupon | They roll back, which releases the coupon lock; the next waiter sees no order and can still win it (`checkouts_that_fail_while_holding_the_coupon_do_not_consume_it`). |
| Empty cart | `409 EMPTY_CART`. |
| Add item racing checkout | The two serialize on the cart row. Either the item is added before checkout reads the cart (and it's in the order), or the add returns `409 CART_ALREADY_CHECKED_OUT`. The order and the cart always agree (`items_added_during_checkout_are_either_ordered_or_rejected`, 15 rounds). |
| Concurrent coupon generation | Serialized by the advisory lock. With two eligible milestones and ten callers: exactly two `201`s (milestones 2 and 4) and eight `409 NO_ELIGIBLE_MILESTONE`s. |
| Repeated report | Identical bodies; table counts and stock unchanged. |

### Multiple instances

Nothing in the design lives in process memory: no caches, counters or idempotency maps. All coordination is row locks, unique constraints and one database-scoped advisory lock, so running N instances against one Postgres keeps every guarantee above unchanged.

---

## 5. Money and rounding rules

- **Currency** is INR. Every amount is an integer number of paise (`i64` in Rust, `BIGINT` in SQL), and every money field in the API ends in `_paise`. No floating point appears anywhere in pricing.
- **Line total** = `unit_price_paise × quantity`, using checked multiplication. The database enforces `order_items_line_total_reconciles`.
- **Subtotal** = the sum of line totals, using checked addition.
- **Discount** = `round_half_up(subtotal × percent / 100)`, computed as `(subtotal × percent + 50) / 100` in `i128`, then capped at the subtotal.
  - Examples: 15 % of 1,010 = 151.5 → 152; 10 % of 999 = 99.9 → 100; 10 % of 994 = 99.4 → 99; 10 % of 5 = 0.5 → 1.
  - It's applied once per order, to the subtotal.
- **Total** = subtotal − discount, which is ≥ 0 by construction. The database enforces `orders_discount_within_subtotal` and `orders_total_reconciles`.
- **Determinism.** The same lines and percent always produce the same result, because nothing depends on time, order of summation or float state.
- **Overflow** returns `422 VALIDATION_FAILED` (field `cart`). It can't wrap.
- **Report revenue** is plain `SUM`s of stored integers (cast `::bigint`). Net revenue equals gross minus discounts exactly, because each order already satisfies `total = subtotal − discount`.

---

## 6. Error model

The choices are explained in the error-model decision above; the full code table with example payloads is in `README.md`. In summary:

- **Every error has a stable `code`.** The HTTP status is the category; `code` is the contract.
- **`details` carry what a client needs to act on the error,** for example `items: [{product_id, requested, available}]`, `expected_subtotal_paise`/`actual_subtotal_paise`, or `orders_until_next_milestone`.
- **Retryable conditions say so** (`retryable: true` on `CONCURRENT_UPDATE` and `SERVICE_UNAVAILABLE`).
- **Malformed (400) is kept distinct from invalid (422) and from conflicting (409),** so a client can decide automatically whether to fix the request, refresh state, or retry.
- **Internals never leak.** `INTERNAL_ERROR` bodies are generic; the cause goes to the log.

---

## 7. Implemented vs intentionally deferred

**Implemented**
- **Products:** list and get. Admin edits for price, name, availability, and a relative stock delta.
- **Carts:** create, view (current prices, purchasability), add, absolute update, remove.
- **Checkout:** a single transaction with required idempotency keys, an optional coupon and an optional price guard.
- **Orders:** `GET /orders/{id}` returns the immutable snapshot; `GET /admin/orders` lists all orders.
- **Coupons:** admin generation, earliest milestone first; admin coupon list with derived status.
- **Reports:** the summary report.
- **Database protections:** constraints and immutability triggers for every invariant the database can express.
- **Test suite:** 27 integration tests against real PostgreSQL (8 of them concurrent), plus 4 money unit tests.
- **Operability:** Docker Compose for Postgres; migrations embedded and applied at startup; request tracing via `tower-http`.

**Deferred, on purpose**
- **Payments.** There is no payment provider; commit is treated as payment success. The required redesign is described in the payment decision and §8.
- **Authentication and authorization.** `/admin/*` routes are administrative by convention only. Carts are bearer capabilities: anyone holding a cart UUID can act on it.
- **Rate limiting and abuse controls,** for example brute-forcing coupon codes: 40 bits is fine against casual guessing but not against an unthrottled attacker.
- **Idempotency key TTL and per-customer scoping.** Keys are kept forever and are global.
- **Server-side retry with backoff on `CONCURRENT_UPDATE`.** The service tells the client the error is retryable but doesn't retry itself.
- **Coupon expiry, voiding, and per-customer assignment.**
- **Pagination** for `/products`, `/admin/orders` and `/admin/coupons`.
- **Metrics and observability** beyond structured request logs: no metrics on lock waits, conflict rates or pool saturation.
- **API documentation format.** There is no OpenAPI document. `README.md` documents every endpoint with `curl` examples, which was the better use of the timebox.
- **Container image.** There is no app container; the app runs with `cargo run` against the Compose Postgres.
- **Config-drift guard for `n`** (see §2).

---

## 8. How the design evolves for production scale

**Multiple service instances.** No change is needed for correctness; see the end of §4. Instances are stateless, and the advisory lock is database-scoped. Two settings need care with **PgBouncer in transaction mode**:
- `pg_advisory_xact_lock` is fine there, because it's transaction-scoped.
- `src/db.rs` sets `lock_timeout` and `statement_timeout` through the `options` startup parameter, which PgBouncer rejects (or silently drops with `ignore_startup_parameters`). They would move to `SET LOCAL` per transaction, or to role-level defaults (`ALTER ROLE … SET`).

**Hot products.** A single popular SKU runs its checkouts one at a time on its product row. Options, in increasing complexity:
- Keep checkout transactions minimal (they already are).
- Split inventory into *k* bucket rows per SKU and decrement any bucket that has stock.
- Move to a reservation model: a short-TTL reservation at "begin checkout", converted to a sale at confirmation, with expired reservations returned by a job.

**Payments.** Order states become `pending → placed | failed`, and inventory and coupon claims become reservations held by the pending order. The payment idempotency key is derived from ours. Only `placed` orders count towards milestones and revenue. A webhook handler confirms or cancels orders, and a reconciler job sweeps orders stuck in `pending`. The idempotency record moves to its own table with recovery points, because the work now spans two systems.

**Outbox and events.** Order placed, coupon earned and coupon redeemed would be written to an outbox table in the same transaction, then published by a relay to email, analytics and fulfilment. Milestone detection could become event-driven: a consumer counts placed orders and *offers* coupon generation, or issues the coupon to the customer who crossed the milestone. `UNIQUE (milestone_order_count)` stays as the guard.

**Background jobs.** Reservation expiry, `pending` payment reconciliation, idempotency-key TTL cleanup, coupon expiry.

**Reporting.** The report is O(all orders) per request. At scale it would read from a replica (accepting some lag) or from aggregates maintained incrementally (per-day, per-product rollups updated from the outbox), and "as of" timestamps would make the lag explicit.

**Identity.** Once customers exist, the following get scoped per customer:
- idempotency keys: `UNIQUE (customer_id, key)`;
- carts: owned by a customer;
- coupons: issued to a customer, so leaked codes are useless to others.

Admin routes would move behind role-based access control, with an audit log of product and coupon operations.

**Data growth.** Orders and order items would be partitioned by month. The global `UNIQUE (idempotency_key)` then has to include the partition key, or move to the separate idempotency table, which is another reason for that table to exist at scale.

---

## 9. Testing strategy

**Unit tests** (`src/money.rs`) cover rounding ties, the 100 % discount at `i64::MAX`, reconciliation and overflow reporting.

**Integration tests** (`tests/it/`, one binary) run against real PostgreSQL. I didn't use SQLite or mocks, because locking and constraint behaviour is precisely what is under test.

**Harness** (`tests/it/common.rs`):
- **Per-test isolation.** Every test creates its own database (`checkout_test_<uuid>`), runs the real migrations, and starts the real router on `127.0.0.1:0`, so tests run in parallel without interfering.
- **Real HTTP.** Every request goes through `reqwest`, with each in-flight request on its own pooled database connection.
- **Concurrency.** Concurrent tests use `#[tokio::test(flavor = "multi_thread", worker_threads = 8)]` and a `race()` helper that releases all tasks together through a `tokio::sync::Barrier`.
- **Assertions on stored state.** Every test checks the database: stock, row counts, which key or coupon is stored. Status codes alone could hide a double write.
- **Cleanup.** Databases are kept after a run so a failure can be inspected, and dropped at the start of the next run.

| File | Tests |
|---|---|
| `carts.rs` | `new_cart_is_open_and_empty`, `unknown_routes_and_methods_use_the_error_envelope`, `invalid_products_and_quantities_never_enter_the_cart`, `items_can_be_added_updated_and_removed_with_correct_totals`, `cart_shows_current_prices_and_flags_lines_that_cannot_be_bought` |
| `checkout.rs` | `checkout_creates_an_order_whose_snapshot_survives_product_changes`, `retrying_with_the_same_key_replays_the_order_without_side_effects`, `reusing_a_key_for_a_different_request_is_rejected`, `a_cart_is_checked_out_at_most_once_even_with_a_new_key`, `invalid_checkout_requests_fail_without_side_effects`, `failed_checkout_leaves_coupon_inventory_and_cart_untouched`, `checkout_charges_the_current_price_unless_the_price_guard_disagrees`, `discontinued_product_blocks_checkout` |
| `concurrency.rs` | `concurrent_checkouts_never_oversell_limited_stock` (20 carts, 5 units), `overlapping_carts_do_not_deadlock_or_oversell` (opposite insertion orders), `concurrent_retries_of_one_checkout_create_exactly_one_order`, `concurrent_checkouts_of_one_cart_with_different_keys_create_one_order`, `a_coupon_is_redeemed_by_at_most_one_of_many_concurrent_checkouts`, `checkouts_that_fail_while_holding_the_coupon_do_not_consume_it`, `concurrent_coupon_generation_never_duplicates_a_milestone`, `items_added_during_checkout_are_either_ordered_or_rejected` |
| `coupons.rs` | `coupon_becomes_available_exactly_at_the_milestone`, `failed_checkouts_do_not_count_towards_milestones`, `delayed_generation_rewards_each_missed_milestone_once_in_order`, `discount_rounds_half_up_and_a_coupon_is_single_use`, `a_full_discount_yields_a_zero_total_never_a_negative_one` |
| `reporting.rs` | `report_reconciles_with_orders_and_coupons_and_never_mutates_state`: recomputes every figure from `GET /admin/orders` and `GET /admin/coupons`, includes a failed checkout that must not appear, and repeats the report 5× while checking that state is unchanged. |

### Verifying the tests can fail

The whole suite passed on its first run. A suite that has never failed proves little, so I mutated the implementation one safeguard at a time, in a scratch copy, and reran `tests/it/concurrency.rs` against each mutant:

| Mutation | Result | What failed |
|---|---|---|
| M1: remove the product row lock **and** turn the decrement into read-modify-write (`SET stock = <read stock> - qty`), the classic lost update | **Caught** | 20 carts racing for 5 units: **20 orders** created (expected 5/15). The overlapping-carts test also oversold. |
| M2: remove only the product row lock; keep the guarded decrement | **Caught** | No oversell: the `stock >= quantity` guard held. But the 15 losers got `500 INTERNAL_ERROR` ("stock changed under lock") instead of `409 INSUFFICIENT_INVENTORY`. |
| M3: remove the coupon `FOR UPDATE` **and** the `orders_coupon_unique` constraint | **Caught** | 10 concurrent checkouts with one code: **2 orders** redeemed the same coupon (expected 1 success, 9 `COUPON_ALREADY_REDEEMED`). |
| M3b: remove only the coupon `FOR UPDATE` | **Not caught** | `orders_coupon_unique` still rejects the second redemption, which maps to `COUPON_ALREADY_REDEEMED`. See the findings in §10. |
| M4: lock products in cart order (`ORDER BY ci.created_at`) instead of by id | **Caught** | The opposite-order carts deadlocked (`409 CONCURRENT_UPDATE`); the test took 7.9 s because of Postgres deadlock detection. |
| M5: remove the cart `FOR UPDATE` | **Caught** | Round 0 of the add-item race: the order had 1 item while the checked-out cart held 2. Concurrent same-key retries got `409 CART_ALREADY_CHECKED_OUT` (from the `orders_cart_unique` backstop) instead of `200` replays. |
| M6: remove the advisory lock from coupon generation | **Caught** | Still no duplicate coupon, because `coupons_milestone_unique` held, but colliding generators got `409 CONCURRENT_UPDATE`: only 3 of the 8 expected callers received a clean `NO_ELIGIBLE_MILESTONE`. |

The same experiment can be repeated by editing the lock clause named in each row and running `cargo test --test it concurrency::`.

---

## 10. How I used AI

I built this with **Claude Code** (Anthropic), working from the assignment brief plus a detailed implementation prompt. I treated that prompt as a starting point, not an authority. The AI wrote most of the first-draft code and documentation. My role was to set the design constraints, review each piece, run it, and redirect it where it was wrong. I have reviewed every line in this repository and I'm accountable for all of it.

These are corrections and redirections that actually happened during the build:

1. **Milestones from a sequence: rejected.** The specification suggested "a stable order sequence" to determine milestones. Postgres sequences are non-transactional: a rolled-back checkout still consumes a number. Order numbers would then have gaps and stop meaning "k-th successful order". I replaced it with counting committed orders while holding an advisory lock (the milestone-counting decision).
2. **A mutable coupon `status` column: replaced.** The suggested schema had `status: available | redeemed` on coupons, updated during checkout. I removed the column and derived the status from `orders.coupon_id UNIQUE`, which removes a whole class of "status disagrees with orders" bugs (the coupon-redemption decision).
3. **SQL built with `format!`: rejected in review.** An early draft of `checkout.rs` had a generic `order_by(conn, predicate: &str, value)` helper that interpolated a WHERE clause with `format!`. The inputs were constants, so it wasn't exploitable, but it's exactly the pattern a reviewer should flag, and it saved nothing. I replaced it with two explicit queries, `order_for_key` and `order_for_cart`.
4. **Price policy: extended.** The specification said checkout "may proceed at the new price". I kept that as the default and added the opt-in `expected_subtotal_paise` guard, so a client can refuse to pay an amount the customer never saw.
5. **Environment: port changed.** The default compose port collided with existing local Postgres instances (5432 was in use, and 5433 failed to bind), so the project uses 54329.

What running the code taught me, as opposed to what I designed up front:

- **The mutation runs showed which layer provides which guarantee.** Without the cart lock (M5), the database constraints still prevented a duplicate order, but the client got the wrong answer: a 409 instead of its own order. Without the product lock (M2), the guarded decrement still prevented overselling, but losers got a 500. Removing the generation advisory lock (M6) showed the same pattern for coupons. So the locks are what make the *error contract* correct; the constraints only keep the *data* correct. I kept both on purpose.
- **One safeguard has no test of its own.** Removing only the coupon lock (M3b) is invisible to the suite, because the unique index catches the second redemption with the same error code. Correctness is unaffected; what the lock buys is that a losing redeemer fails before taking any product locks. I documented this instead of writing a timing-dependent test to force it.
- **The README walkthrough was executed verbatim** against a fresh database. Every annotated result (replay headers, 409/422 codes, coupon arithmetic, price guard, report totals) matched. While reviewing the docs I found that a wrong HTTP method returned Axum's empty-bodied 405, outside the error envelope. I fixed it with `method_not_allowed_fallback` and added `unknown_routes_and_methods_use_the_error_envelope`.

---

## 11. What I would examine first with two more hours

1. **Load-test a single hot SKU.** Run a few hundred concurrent checkouts and measure p99 latency, lock-wait time and how often `CONCURRENT_UPDATE` from `lock_timeout` occurs. That would tell me whether 5 s is the right timeout and whether checkout needs a server-side bounded retry.
2. **Guard the reward configuration.** Store `n`/`x` in a `reward_settings` table on first boot, and refuse to start (or require an explicit migration) if the environment disagrees. That closes the "changing `n` double-rewards" hole described in §2.
3. **Idempotency key lifecycle.** Add a TTL and cleanup job, and decide on per-customer scoping. Revisit whether to persist failed outcomes once payments exist.
4. **Coupon brute-force resistance.** Add rate limiting on checkouts that fail with `UNKNOWN_COUPON`, and use longer codes.
5. **Generated OpenAPI plus a CI workflow.** Generate the API document with `utoipa`, and add a GitHub Actions job that runs `fmt`, `clippy` and the suite against a Postgres service container.
6. **Property-based tests for `money.rs`** (proptest): `0 <= discount <= subtotal`, `total = subtotal - discount`, and monotonicity in percent.

---

Approximate time spent: **TODO (candidate): fill in your actual hours before submitting.**
