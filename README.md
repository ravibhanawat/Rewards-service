# Reliable Checkout and Rewards Service

The backend for a small store. Customers build carts and check out. Every *n*-th successful order earns a coupon worth *x* % off, and an administrator generates it.

The interesting part is not the CRUD. It is what happens when requests are **retried**, **overlap**, or **compete** for the same stock or coupon. The design reasoning is in **[DECISIONS.md](DECISIONS.md)**. This README covers how to run, test, and call the service.

**Stack:** Rust (edition 2024) · Axum 0.8 · Tokio · SQLx 0.8 · PostgreSQL 17 · Docker Compose

```
src/
  main.rs            bootstrap: config, pool, migrations, server
  routes.rs          HTTP handlers (no business rules)
  services/          one module per use case; each owns its transaction
    carts.rs  checkout.rs  coupons.rs  orders.rs  products.rs  reports.rs
  money.rs           integer-paise arithmetic + rounding (unit tested)
  error.rs           single error type and its HTTP mapping
  extract.rs         Json/Path extractors with structured rejections
migrations/          schema, constraints, immutability triggers, seed data
tests/it/            integration + concurrency tests against real Postgres
```

## Key invariants and where they are enforced

| Invariant | Application | Database backstop |
|---|---|---|
| Inventory is never oversold | `services/checkout.rs` locks product rows `FOR NO KEY UPDATE` in product-id order, validates, then runs a guarded decrement (`stock >= quantity`) | `products_stock_non_negative` CHECK |
| A cart produces at most one order | Cart row `FOR UPDATE` in checkout; cart mutations take the same row lock (`lock_open_cart` in `services/carts.rs`) | `orders_cart_unique` |
| A retry never creates a second order | The order row stores the `Idempotency-Key` and a request fingerprint; replays are served from it (`order_for_key` / `replay` in `services/checkout.rs`) | `orders_idempotency_key_unique` |
| A coupon is redeemed at most once | Coupon row locked `FOR UPDATE`, then redemption re-checked against committed orders (`lock_unredeemed_coupon`) | `orders_coupon_unique` |
| A failed checkout consumes nothing | Checkout is a single transaction; coupon status is *derived* from `orders.coupon_id`, so a rollback leaves it available | — |
| One coupon per milestone | `pg_advisory_xact_lock` serializes generators; milestone = committed order count (`services/coupons.rs`) | `coupons_milestone_unique` |
| Orders explain their own totals forever | `order_items` snapshots name, unit price, and line total; the order snapshots coupon code and percent | `order_items_line_total_reconciles`, `orders_total_reconciles`, `orders_discount_within_subtotal`, `orders_coupon_snapshot_complete`; triggers `orders_immutable`, `order_items_immutable`, `coupons_immutable` |
| Money has no rounding drift | Integer paise (`i64`), checked arithmetic, one half-up rounding per order (`money.rs`) | `BIGINT` columns |
| The report reconciles and never mutates | One `REPEATABLE READ, READ ONLY` snapshot; separate aggregates so joins cannot multiply totals (`services/reports.rs`) | — |

## Prerequisites

- **Rust** stable **1.88+**, because the code uses edition 2024 and `let` chains. It was developed with 1.96.
- **Docker** with the Compose plugin, which provides PostgreSQL. No other services or credentials are needed.

## Setup and run

```bash
docker compose up -d --wait     # PostgreSQL 17 on host port 54329
cp .env.example .env            # local settings; no real credentials
cargo run                       # applies migrations + seed data, then listens on 127.0.0.1:8080
```

Migrations, including the seed products, are embedded in the binary and applied at startup. No `sqlx-cli` is needed.

Reset to a fresh database with `docker compose down -v && docker compose up -d --wait`.

### Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | *(required)* | e.g. `postgres://checkout:checkout@localhost:54329/checkout` |
| `BIND_ADDR` | `127.0.0.1:8080` | HTTP listen address |
| `DB_MAX_CONNECTIONS` | `20` | Pool size |
| `REWARD_EVERY_N_ORDERS` | `5` | *n*: every *n*-th successful order earns a coupon (must be ≥ 1) |
| `REWARD_DISCOUNT_PERCENT` | `10` | *x*: coupon discount in percent (1–100) |
| `RUST_LOG` | `info,sqlx=warn` | Log filter (`tracing-subscriber` `EnvFilter` syntax) |

Every pooled connection also sets `lock_timeout = 5s` and `statement_timeout = 15s` (see `src/db.rs`). Under heavy contention a request then fails with a retryable `409 CONCURRENT_UPDATE` instead of hanging (lock timeout, statement timeout, deadlock and serialization failures all map to it).

## Running the tests

```bash
docker compose up -d --wait     # tests need the compose Postgres
cargo test                      # unit + integration tests
```

- Each integration test creates its own database, `checkout_test_<uuid>`, runs the migrations, and starts the real server on a random port. Tests run in parallel and requests go over real HTTP.
- Test databases are kept after the run so you can inspect a failure. The next run drops them before it starts.
- The tests honour `DATABASE_URL` (from the environment or `.env`). Without it they use `postgres://checkout:checkout@localhost:54329/checkout`. The database user needs `CREATEDB`; the compose user is a superuser.

| File | Covers |
|---|---|
| `src/money.rs` (unit) | Half-up rounding, discount ≤ subtotal, zero totals, overflow is reported rather than wrapped |
| `tests/it/carts.rs` | Empty cart; invalid products and quantities rejected without side effects; add/update/remove totals; current-price display; admin stock adjustments bounded and never negative |
| `tests/it/checkout.rs` | Order snapshot survives product edits; DB rejects tampering; same-key replay; key reuse → 422; second key → 409; failed checkout leaves coupon, stock, and cart untouched; price guard; discontinued products |
| `tests/it/coupons.rs` | Exact milestone boundary; failed checkouts don't count; delayed generation rewards each milestone once, in order; half-up discount; single use; 100 % coupon → total 0 |
| `tests/it/concurrency.rs` | Competing requests released together through a barrier on a multi-threaded runtime (below) |
| `tests/it/reporting.rs` | Report equals totals recomputed from `/admin/orders` and `/admin/coupons`; failed checkouts excluded; repeated calls are identical and mutate nothing |

What the concurrency tests prove, each by asserting final database state:

- **20 carts race for 5 units:** exactly 5 orders, 15 `INSUFFICIENT_INVENTORY`, stock ends at 0, units sold = initial stock.
- **20 carts with the same two products added in opposite orders:** no deadlocks (`CONCURRENT_UPDATE`) and no oversell.
- **10 concurrent retries with one `Idempotency-Key`:** one `201`, nine `200` replays of the same order id, and stock decremented once.
- **10 concurrent checkouts of one cart with different keys:** one order, nine `CART_ALREADY_CHECKED_OUT`.
- **10 concurrent checkouts with one coupon:** exactly one redemption, and the losers' carts and stock are untouched.
- **Nine checkouts that lock the coupon and then fail on stock, plus one valid checkout:** the valid one always gets the coupon.
- **10 concurrent coupon generations with 2 eligible milestones:** exactly the coupons for milestones {2, 4}.
- **"Add item" racing checkout:** the order always contains exactly what the cart contains.
- **One `Idempotency-Key` sent for two different carts at the same instant (10 rounds):** exactly one order per round, the other request gets `422 IDEMPOTENCY_KEY_REUSED`, and its cart stays open.
- **Reports read while 30 checkouts commit:** every report is internally consistent (per-product revenue equals order totals in the same response).

## Seed data

| ID | Name | Price (paise) | Price (₹) | Stock |
|---|---|---:|---:|---:|
| `00000000-0000-4000-8000-000000000001` | Mechanical Keyboard | 349900 | 3,499.00 | 50 |
| `00000000-0000-4000-8000-000000000002` | Wireless Mouse | 129950 | 1,299.50 | 100 |
| `00000000-0000-4000-8000-000000000003` | USB-C Cable (1m) | 39900 | 399.00 | 500 |
| `00000000-0000-4000-8000-000000000004` | Laptop Stand | 189900 | 1,899.00 | 25 |
| `00000000-0000-4000-8000-000000000005` | **Limited Edition Desk Mat** (limited) | 99900 | 999.00 | **3** |
| `00000000-0000-4000-8000-000000000006` | Sticker Pack | 4999 | 49.99 | 200 |

## API reference

**Conventions**
- JSON in, JSON out.
- **All money fields are integer paise** (suffix `_paise`), and the currency is **INR**: ₹25.50 = `2550`.
- IDs are UUIDs and timestamps are RFC 3339 UTC.
- Request bodies reject unknown fields (`400`).
- **Administrative endpoints are everything under `/admin/*`.** Authentication and authorization are out of scope, so these routes are open in this exercise.

### Error envelope

Every error has the same shape. Clients should branch on `error.code`; the HTTP status is only the category.

```json
{
  "error": {
    "code": "INSUFFICIENT_INVENTORY",
    "message": "requested quantity exceeds available stock",
    "details": { "items": [ { "product_id": "…", "requested": 5, "available": 2 } ] }
  }
}
```

`details` is present only where it adds information. A `500` never includes internals; the cause is logged server-side.

**Status policy**

| Status | Meaning |
|---|---|
| 400 | Malformed request: bad JSON or field types, unknown fields, bad path UUID, missing or invalid header |
| 404 | The resource addressed by the URL does not exist |
| 409 | The request is valid but conflicts with current server state. It may succeed once that state changes |
| 422 | The request is well-formed but its content is invalid on its own: quantity out of range, unknown product or coupon, reused idempotency key |
| 503 | Overloaded (connection pool exhausted); retry |
| 500 | Unexpected |

### Error codes

| Code | Status | When | `details` |
|---|---|---|---|
| `INVALID_REQUEST` | 400 | Malformed JSON, wrong types, unknown fields, unparseable path UUID | — |
| `IDEMPOTENCY_KEY_MISSING` | 400 | Checkout without an `Idempotency-Key` header | — |
| `IDEMPOTENCY_KEY_INVALID` | 400 | Key empty, longer than 255, or not visible ASCII | — |
| `VALIDATION_FAILED` | 422 | Quantity outside 1–1000, blank coupon code, invalid admin product edit, cart total overflow | `field` |
| `UNKNOWN_PRODUCT` | 422 | `product_id` in an add-item body does not exist | `product_id` |
| `UNKNOWN_COUPON` | 422 | `coupon_code` does not exist | `coupon_code` |
| `IDEMPOTENCY_KEY_REUSED` | 422 | Key already produced an order for a *different* request (other cart, coupon, or price guard) | — |
| `CART_NOT_FOUND` | 404 | Cart id does not exist | `cart_id` |
| `PRODUCT_NOT_FOUND` | 404 | Product id in the URL does not exist | `product_id` |
| `ORDER_NOT_FOUND` | 404 | Order id does not exist | `order_id` |
| `ITEM_NOT_IN_CART` | 404 | PATCH/DELETE of a product that is not in the cart | `cart_id`, `product_id` |
| `ROUTE_NOT_FOUND` | 404 | No such path | — |
| `METHOD_NOT_ALLOWED` | 405 | Known path, wrong HTTP method | — |
| `CART_ALREADY_CHECKED_OUT` | 409 | Mutating, or checking out with a new key, a cart that already has an order | `cart_id` |
| `ITEM_ALREADY_IN_CART` | 409 | POST of a product already in the cart (use PATCH) | `product_id` |
| `EMPTY_CART` | 409 | Checkout of a cart with no items | — |
| `INSUFFICIENT_INVENTORY` | 409 | Quantity exceeds stock (add, update, checkout), or an admin stock decrease below 0 | `items: [{product_id, requested, available}]` |
| `PRODUCT_UNAVAILABLE` | 409 | Product discontinued (`active = false`) | `product_ids` |
| `COUPON_ALREADY_REDEEMED` | 409 | Coupon already used by an order | `coupon_code` |
| `PRICE_CHANGED` | 409 | `expected_subtotal_paise` differs from the subtotal at current prices | `expected_subtotal_paise`, `actual_subtotal_paise` |
| `NO_ELIGIBLE_MILESTONE` | 409 | Coupon generation requested but no reached milestone is unrewarded | `successful_orders`, `every_n_orders`, `next_milestone_order_count`, `orders_until_next_milestone` |
| `CONCURRENT_UPDATE` | 409 | Lock timeout, deadlock, or serialization failure. Nothing was applied; safe to retry | `retryable: true` |
| `SERVICE_UNAVAILABLE` | 503 | Connection pool exhausted; safe to retry | `retryable: true` |
| `INTERNAL_ERROR` | 500 | Unexpected failure | — |

A wrong HTTP method on a known path returns `405` with code `METHOD_NOT_ALLOWED`, in the same envelope.

---

### `GET /health`

Returns `200` with the plain-text body `ok`.

### `GET /products`

Returns `200` with every product, sorted by name.

```json
[
  {
    "id": "00000000-0000-4000-8000-000000000005",
    "name": "Limited Edition Desk Mat",
    "price_paise": 99900,
    "stock": 3,
    "active": true,
    "created_at": "2026-10-17T05:44:18.921075Z",
    "updated_at": "2026-10-17T05:44:18.921075Z"
  }
]
```

### `GET /products/{product_id}`

Returns `200` with one product (same shape as above).

Errors: `400 INVALID_REQUEST` (bad UUID), `404 PRODUCT_NOT_FOUND`.

### `POST /carts`

No body. Returns `201` with a `Location: /carts/{id}` header and the empty cart:

```json
{
  "id": "977cf611-3e01-49a4-a50e-af3a80f8d79b",
  "status": "open",
  "currency": "INR",
  "items": [],
  "total_quantity": 0,
  "subtotal_paise": 0,
  "order_id": null,
  "created_at": "2026-10-17T06:00:00Z",
  "updated_at": "2026-10-17T06:00:00Z"
}
```

### `GET /carts/{cart_id}`

Returns `200` with the cart.

- Prices are **current product prices** and are **indicative**. The price actually charged is fixed at checkout and recorded on the order.
- `purchasable: false` means checkout would currently fail for that line: the product was discontinued or stock fell below the quantity.
- `order_id` is set once the cart is checked out.

```json
{
  "id": "977cf611-…",
  "status": "open",
  "currency": "INR",
  "items": [
    {
      "product_id": "00000000-0000-4000-8000-000000000005",
      "name": "Limited Edition Desk Mat",
      "unit_price_paise": 99900,
      "quantity": 2,
      "line_total_paise": 199800,
      "stock_available": 3,
      "purchasable": true
    }
  ],
  "total_quantity": 2,
  "subtotal_paise": 199800,
  "order_id": null,
  "created_at": "…",
  "updated_at": "…"
}
```

Errors: `400 INVALID_REQUEST`, `404 CART_NOT_FOUND`.

### `POST /carts/{cart_id}/items`

Adds a new line.

```json
{ "product_id": "00000000-0000-4000-8000-000000000005", "quantity": 2 }
```

Returns `201` with the updated cart.

Adding a product that is already in the cart is a `409` rather than an increment, so a retried "add" can never double the quantity. Use PATCH to change a quantity.

Stock is checked at add time to reject obviously impossible lines. This check is **advisory, not a reservation**: checkout re-validates under row locks.

| Error | When |
|---|---|
| `400 INVALID_REQUEST` | Non-integer quantity, bad UUID, missing or unknown field |
| `404 CART_NOT_FOUND` | Cart id does not exist |
| `409 CART_ALREADY_CHECKED_OUT` | Cart already has an order |
| `409 ITEM_ALREADY_IN_CART` | Product already in the cart |
| `409 INSUFFICIENT_INVENTORY` | Quantity exceeds current stock |
| `409 PRODUCT_UNAVAILABLE` | Product discontinued |
| `422 VALIDATION_FAILED` | Quantity not in 1–1000 |
| `422 UNKNOWN_PRODUCT` | Product does not exist |

### `PATCH /carts/{cart_id}/items/{product_id}`

Sets the **absolute** quantity, so the request is idempotent.

```json
{ "quantity": 4 }
```

Returns `200` with the updated cart. Use DELETE to remove a line; `quantity: 0` is a `422`.

Errors: `404 CART_NOT_FOUND`, `404 ITEM_NOT_IN_CART` (the product is not in this cart), `409 CART_ALREADY_CHECKED_OUT`, `422 VALIDATION_FAILED` (quantity outside 1–1000), `409 INSUFFICIENT_INVENTORY` / `409 PRODUCT_UNAVAILABLE` (the new quantity can't currently be bought), `400 INVALID_REQUEST`. To remove a line, use `DELETE`.

### `DELETE /carts/{cart_id}/items/{product_id}`

No body. Returns `200` with the updated cart.

Errors: `404 CART_NOT_FOUND`, `404 ITEM_NOT_IN_CART` (including a repeated delete), `409 CART_ALREADY_CHECKED_OUT`.

### `POST /carts/{cart_id}/checkout`

| | |
|---|---|
| **Header** | `Idempotency-Key: <1–255 visible ASCII chars>` (**required**). Generate one per checkout intent and reuse it on retries. |
| **Body** | Optional. An empty body means "no coupon, no price guard". |

```json
{ "coupon_code": "RWD-7F3A9C0B1D", "expected_subtotal_paise": 199800 }
```

- `coupon_code` is optional, case-insensitive, and trimmed. One coupon per order.
- `expected_subtotal_paise` is an optional price guard. If prices changed since the client last viewed the cart, checkout fails with `409 PRICE_CHANGED` instead of charging an amount the customer has not seen. Without the guard, checkout charges the current price, and the order records exactly what was charged.

**Outcomes**

| Situation | Response |
|---|---|
| New checkout succeeds | `201 Created`, `Location: /orders/{id}`, order body |
| Same key and same payload already produced an order, including concurrent duplicates | `200 OK`, `Idempotent-Replayed: true`, the **same** order body. Nothing is re-executed |
| Same key with a different payload (other cart, coupon, or guard) | `422 IDEMPOTENCY_KEY_REUSED` |
| Different key on a cart that is already checked out | `409 CART_ALREADY_CHECKED_OUT` |
| Any failure | No effects at all. Stock, coupon, and cart are unchanged, the key is **not** bound, and the same key may be retried after fixing the cause |

Order body (`201` or `200`):

```json
{
  "id": "b62b11b5-b4a1-43fb-8d6f-c6b8ff80ec8c",
  "cart_id": "977cf611-3e01-49a4-a50e-af3a80f8d79b",
  "status": "placed",
  "currency": "INR",
  "items": [
    {
      "product_id": "00000000-0000-4000-8000-000000000005",
      "product_name": "Limited Edition Desk Mat",
      "unit_price_paise": 99900,
      "quantity": 2,
      "line_total_paise": 199800
    }
  ],
  "subtotal_paise": 199800,
  "coupon": { "code": "RWD-7F3A9C0B1D", "discount_percent": 10 },
  "discount_paise": 19980,
  "total_paise": 179820,
  "created_at": "2026-10-17T06:01:00Z"
}
```

`coupon` is `null` and `discount_paise` is `0` when no coupon was used.

The math is `subtotal = Σ line_total`, `discount = round_half_up(subtotal × percent / 100)` (computed once per order and never above the subtotal), and `total = subtotal − discount ≥ 0`.

| Error | When |
|---|---|
| `400 IDEMPOTENCY_KEY_MISSING` / `IDEMPOTENCY_KEY_INVALID` | Header absent or malformed |
| `400 INVALID_REQUEST` | Body is not valid JSON, has wrong types, or has unknown fields |
| `404 CART_NOT_FOUND` | Cart id does not exist |
| `409 CART_ALREADY_CHECKED_OUT` | Cart already has an order (different key) |
| `409 EMPTY_CART` | Cart has no items |
| `409 PRODUCT_UNAVAILABLE` | A product was discontinued |
| `409 INSUFFICIENT_INVENTORY` | Stock is short; every short line is listed |
| `409 COUPON_ALREADY_REDEEMED` | Coupon already used |
| `409 PRICE_CHANGED` | Price guard does not match |
| `409 CONCURRENT_UPDATE` | Lock timeout or similar; retry with the same key |
| `422 VALIDATION_FAILED` | Blank or over-long coupon code |
| `422 UNKNOWN_COUPON` | Coupon does not exist |
| `422 IDEMPOTENCY_KEY_REUSED` | Key already used for a different request |

### `GET /orders/{order_id}`

Returns `200` with the order body shown above. It is immutable: later product renames or price changes do not affect it.

Errors: `400 INVALID_REQUEST`, `404 ORDER_NOT_FOUND`.

---

### Administrative endpoints (unauthenticated in this exercise)

#### `POST /admin/coupons/generate`

No body. Generates the coupon for the **earliest** milestone that has been reached but not yet rewarded. It creates at most one coupon per call; call again for further milestones.

Milestone *k* is reached when there are *k × n* successfully placed (committed) orders.

Returns `201`:

```json
{
  "coupon": {
    "id": "5e0d…",
    "code": "RWD-7F3A9C0B1D",
    "milestone_order_count": 5,
    "discount_percent": 10,
    "status": "available",
    "redeemed_order_id": null,
    "redeemed_at": null,
    "created_at": "…"
  },
  "successful_orders": 5,
  "remaining_eligible_milestones": 0
}
```

| Error | When |
|---|---|
| `409 NO_ELIGIBLE_MILESTONE` | No reached milestone is unrewarded. Example details: `{"successful_orders": 1, "every_n_orders": 5, "next_milestone_order_count": 5, "orders_until_next_milestone": 4}` |
| `409 CONCURRENT_UPDATE` | Retryable |

#### `GET /admin/coupons`

Returns `200` with all coupons ordered by milestone (shape as `coupon` above).

`status` is `available` or `redeemed`. It is derived from whether an order references the coupon. When redeemed, `redeemed_order_id` and `redeemed_at` are set.

#### `GET /admin/orders`

Returns `200` with every order, oldest first, including items (order body shape). It is unpaginated and is provided so the report can be reconciled.

#### `GET /admin/reports/summary`

Returns `200`. The report is read-only and computed from committed orders, order items, and coupons in one consistent snapshot. Repeated calls with no new activity return identical bodies.

```json
{
  "currency": "INR",
  "successful_orders": 4,
  "gross_revenue_paise": 116994,
  "total_discounts_paise": 7200,
  "net_revenue_paise": 109794,
  "coupons": { "generated": 2, "available": 1, "redeemed": 1 },
  "products": [
    { "product_id": "…", "name": "A", "quantity_sold": 6, "gross_revenue_paise": 11994 }
  ]
}
```

- `net_revenue_paise = gross_revenue_paise − total_discounts_paise`.
- `available + redeemed = generated`.
- `products` lists every product, including unsold ones (`0`). `gross_revenue_paise` per product is before discounts and sums to the overall gross.

#### `PATCH /admin/products/{product_id}`

Edits a product, so you can exercise price and availability changes between "add to cart" and checkout. All fields are optional, but at least one is required.

```json
{ "name": "Desk Mat v2", "price_paise": 109900, "stock_delta": 5, "active": true }
```

`stock_delta` is a **relative** adjustment (restock or write-off), limited to ±1,000,000 per call. An absolute "set stock to N" would silently overwrite decrements from checkouts that commit between reading and writing.

Returns `200` with the updated product.

| Error | When |
|---|---|
| `422 VALIDATION_FAILED` | `{}` (no fields), name not 1–200 chars, `price_paise` outside 0–100,000,000,000, or `stock_delta` outside ±1,000,000 |
| `404 PRODUCT_NOT_FOUND` | Product id does not exist |
| `409 INSUFFICIENT_INVENTORY` | The delta would make stock negative |
| `400 INVALID_REQUEST` | Missing or malformed JSON body, or unknown field |

## curl walkthrough

The walkthrough runs against a fresh database (`docker compose down -v && docker compose up -d --wait && cargo run`) with the default `n = 5`, `x = 10`.

It uses `jq` only to pull ids out of responses. Without `jq`, drop the `| jq …` parts and paste the ids by hand.

```bash
BASE=http://localhost:8080
MAT=00000000-0000-4000-8000-000000000005        # limited stock: 3
KEYBOARD=00000000-0000-4000-8000-000000000001
STICKERS=00000000-0000-4000-8000-000000000006
JSON='Content-Type: application/json'

# Products
curl -s $BASE/products | jq

# Cart: create, add, change quantity, add + remove another line, view
CART=$(curl -s -X POST $BASE/carts | jq -r .id); echo "cart $CART"
curl -s -X POST  $BASE/carts/$CART/items -H "$JSON" -d "{\"product_id\":\"$MAT\",\"quantity\":1}" | jq .subtotal_paise
curl -s -X PATCH $BASE/carts/$CART/items/$MAT -H "$JSON" -d '{"quantity":2}' | jq .subtotal_paise
curl -s -X POST  $BASE/carts/$CART/items -H "$JSON" -d "{\"product_id\":\"$STICKERS\",\"quantity\":3}" | jq .total_quantity
curl -s -X DELETE $BASE/carts/$CART/items/$STICKERS | jq .items
curl -s $BASE/carts/$CART | jq

# Validation and conflict examples
curl -s -X POST $BASE/carts/$CART/items -H "$JSON" -d "{\"product_id\":\"$MAT\",\"quantity\":1}" | jq .error.code   # ITEM_ALREADY_IN_CART
curl -s -X PATCH $BASE/carts/$CART/items/$MAT -H "$JSON" -d '{"quantity":0}' | jq .error.code                      # VALIDATION_FAILED

# Checkout without a coupon, then retry with the same key (replay), then a new key (409)
KEY=$(uuidgen)
curl -s -i -X POST $BASE/carts/$CART/checkout -H "Idempotency-Key: $KEY"           # 201 + Location
curl -s -i -X POST $BASE/carts/$CART/checkout -H "Idempotency-Key: $KEY" | head -5  # 200 + Idempotent-Replayed: true
curl -s -X POST $BASE/carts/$CART/checkout -H "Idempotency-Key: $(uuidgen)" | jq .error.code   # CART_ALREADY_CHECKED_OUT
curl -s -X POST $BASE/carts/$CART/checkout -H "Idempotency-Key: $KEY" -d '{"coupon_code":"X"}' | jq .error.code  # IDEMPOTENCY_KEY_REUSED

# Reach the first milestone (n = 5): ask how many orders are missing, then place them
place_order() {   # one Sticker Pack per order
  local cart; cart=$(curl -s -X POST $BASE/carts | jq -r .id)
  curl -s -X POST $BASE/carts/$cart/items -H "$JSON" -d "{\"product_id\":\"$STICKERS\",\"quantity\":1}" > /dev/null
  curl -s -X POST $BASE/carts/$cart/checkout -H "Idempotency-Key: $(uuidgen)" | jq -c '{id, total_paise}'
}
curl -s -X POST $BASE/admin/coupons/generate | jq .error     # NO_ELIGIBLE_MILESTONE, orders_until_next_milestone: 4
for i in 1 2 3 4; do place_order; done

# Generate the coupon and use it
COUPON=$(curl -s -X POST $BASE/admin/coupons/generate | jq -r .coupon.code); echo "coupon $COUPON"
CART2=$(curl -s -X POST $BASE/carts | jq -r .id)
curl -s -X POST $BASE/carts/$CART2/items -H "$JSON" -d "{\"product_id\":\"$KEYBOARD\",\"quantity\":1}" > /dev/null
ORDER=$(curl -s -X POST $BASE/carts/$CART2/checkout -H "Idempotency-Key: $(uuidgen)" -H "$JSON" \
          -d "{\"coupon_code\":\"$COUPON\"}" | jq -r .id)
curl -s $BASE/orders/$ORDER | jq '{subtotal_paise, coupon, discount_paise, total_paise}'
# subtotal 349900, 10% -> discount 34990, total 314910

# Reusing the coupon fails and changes nothing
CART3=$(curl -s -X POST $BASE/carts | jq -r .id)
curl -s -X POST $BASE/carts/$CART3/items -H "$JSON" -d "{\"product_id\":\"$KEYBOARD\",\"quantity\":1}" > /dev/null
curl -s -X POST $BASE/carts/$CART3/checkout -H "Idempotency-Key: $(uuidgen)" -H "$JSON" \
     -d "{\"coupon_code\":\"$COUPON\"}" | jq .error.code      # COUPON_ALREADY_REDEEMED

# Price change between "add to cart" and checkout, with the price guard
curl -s $BASE/carts/$CART3 | jq .subtotal_paise                                   # 349900
curl -s -X PATCH $BASE/admin/products/$KEYBOARD -H "$JSON" -d '{"price_paise":359900}' | jq .price_paise
curl -s -X POST $BASE/carts/$CART3/checkout -H "Idempotency-Key: $(uuidgen)" -H "$JSON" \
     -d '{"expected_subtotal_paise":349900}' | jq .error       # PRICE_CHANGED, actual 359900
curl -s -X POST $BASE/carts/$CART3/checkout -H "Idempotency-Key: $(uuidgen)" -H "$JSON" \
     -d '{"expected_subtotal_paise":359900}' | jq '{total_paise, items}'

# Administration
curl -s $BASE/admin/reports/summary | jq
curl -s $BASE/admin/orders  | jq 'length'
curl -s $BASE/admin/coupons | jq
```

To watch inventory contention over real HTTP, `scripts/race-demo.sh [N]` creates N carts that each hold one Desk Mat and fires all N checkouts in parallel. It needs only curl and python3. Run it on a fresh database (stock 3), before the walkthrough above has bought any mats, because its carts can only be filled while stock lasts. Exactly as many checkouts succeed as there are units, and stock ends at 0:

```text
$ scripts/race-demo.sh 10
stock before: 3
  HTTP    3 201
  HTTP    7 409
stock after:  0
```

## Known limitations

- **No authentication or authorization.** `/admin/*` is open, and carts are bearer capabilities: anyone with a cart id can use it.
- **No real payment integration.** A committed checkout is treated as a successful payment. A real payment provider would need a pending-order or reservation step and reconciliation (see DECISIONS.md).
- **No stock reservation at add-to-cart.** Stock is only taken at checkout, and abandoned carts never expire.
- **Coupons are global bearer codes.** Anyone holding a code can redeem it once. There is one coupon per order, it applies to the whole subtotal, and it never expires.
- ***n* and *x* are startup configuration.** Changing *x* affects only coupons generated afterwards. Changing *n* on a database that already has coupons is not reconciled retroactively; milestones are recomputed as multiples of the new *n*.
- **`/admin/orders` and `/admin/coupons` are unpaginated**, and the report is computed on demand from the raw tables.
- **A single PostgreSQL primary is assumed.** Correctness relies on its row locks, advisory lock, and constraints, so several app instances can share it safely, but there is no multi-region story.
- **No app container.** The service runs with `cargo run`; only PostgreSQL runs in Docker.

Approximate time spent: see DECISIONS.md.
