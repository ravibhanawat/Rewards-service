//! Checkout: one PostgreSQL transaction that validates the cart, prices it,
//! decrements inventory, redeems an optional coupon and records the order.
//! Either all of it commits or none of it does.
//!
//! Lock order (always the same, so concurrent checkouts cannot deadlock):
//!   1. the cart row        `FOR UPDATE`          serializes work on one cart
//!   2. the coupon row      `FOR UPDATE`          serializes redeemers of one coupon
//!   3. product rows        `FOR NO KEY UPDATE`   in ascending product id order
//!
//! Idempotency: the order row stores the Idempotency-Key and a fingerprint of
//! the request. Because it is written in the same transaction as every other
//! effect, "the key was used" and "the order exists" are the same fact.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult, Shortfall, violated_constraint},
    models::OrderView,
    money::{self, Paise},
    services::orders,
};

pub const MAX_IDEMPOTENCY_KEY_LEN: usize = 255;
const MAX_COUPON_CODE_LEN: usize = 64;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckoutRequest {
    pub coupon_code: Option<String>,
    /// Optional price guard: if set and the subtotal computed from current
    /// prices differs, checkout fails with PRICE_CHANGED instead of charging
    /// an amount the customer has not seen.
    pub expected_subtotal_paise: Option<Paise>,
}

#[derive(Debug)]
pub enum CheckoutOutcome {
    Created(OrderView),
    /// The same key and payload already produced this order.
    Replayed(OrderView),
}

#[derive(FromRow)]
struct OrderIdentity {
    id: Uuid,
    idempotency_key: String,
    request_fingerprint: String,
}

#[derive(FromRow)]
struct LockedCoupon {
    id: Uuid,
    code: String,
    discount_percent: i32,
}

#[derive(FromRow)]
struct LockedLine {
    product_id: Uuid,
    name: String,
    price_paise: Paise,
    stock: i32,
    active: bool,
    quantity: i32,
}

pub async fn checkout(
    pool: &PgPool,
    cart_id: Uuid,
    idempotency_key: &str,
    request: CheckoutRequest,
) -> AppResult<CheckoutOutcome> {
    let coupon_code = request.coupon_code.as_deref().map(normalize_coupon_code).transpose()?;
    let fingerprint = fingerprint(cart_id, coupon_code.as_deref(), request.expected_subtotal_paise);

    let mut tx = pool.begin().await?;

    // A key that already produced an order is answered from that order.
    if let Some(existing) = order_for_key(&mut tx, idempotency_key).await? {
        return replay(&mut tx, existing, &fingerprint).await;
    }

    // 1. Cart lock. A concurrent checkout of the same cart (retry or not)
    //    waits here, then sees the committed outcome of the first one.
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM carts WHERE id = $1 FOR UPDATE")
        .bind(cart_id)
        .fetch_optional(&mut *tx)
        .await?;
    match status.as_deref() {
        None => return Err(AppError::CartNotFound(cart_id)),
        Some("open") => {}
        Some(_) => {
            // A duplicate that lost the lock race to the request carrying the
            // same key: answer with the order that request created.
            let existing = order_for_cart(&mut tx, cart_id)
                .await?
                .ok_or_else(|| AppError::Internal(format!("checked-out cart {cart_id} has no order")))?;
            if existing.idempotency_key == idempotency_key {
                return replay(&mut tx, existing, &fingerprint).await;
            }
            return Err(AppError::CartAlreadyCheckedOut(cart_id));
        }
    }

    // 2. Coupon lock, before product locks: a losing redeemer fails fast
    //    without ever holding (and blocking others on) hot product rows.
    let coupon = match &coupon_code {
        Some(code) => Some(lock_unredeemed_coupon(&mut tx, code).await?),
        None => None,
    };

    // 3. Product locks in id order; validate everything before changing anything.
    let lines: Vec<LockedLine> = sqlx::query_as(
        "SELECT p.id AS product_id, p.name, p.price_paise, p.stock, p.active, ci.quantity
         FROM cart_items ci JOIN products p ON p.id = ci.product_id
         WHERE ci.cart_id = $1
         ORDER BY p.id
         FOR NO KEY UPDATE OF p",
    )
    .bind(cart_id)
    .fetch_all(&mut *tx)
    .await?;
    if lines.is_empty() {
        return Err(AppError::EmptyCart);
    }
    let unavailable: Vec<Uuid> = lines.iter().filter(|l| !l.active).map(|l| l.product_id).collect();
    if !unavailable.is_empty() {
        return Err(AppError::ProductUnavailable(unavailable));
    }
    let shortfalls: Vec<Shortfall> = lines
        .iter()
        .filter(|l| l.quantity > l.stock)
        .map(|l| Shortfall { product_id: l.product_id, requested: l.quantity, available: l.stock })
        .collect();
    if !shortfalls.is_empty() {
        return Err(AppError::InsufficientInventory(shortfalls));
    }

    let priced: Vec<(Paise, i32)> = lines.iter().map(|l| (l.price_paise, l.quantity)).collect();
    let price = money::price_order(&priced, coupon.as_ref().map(|c| c.discount_percent))?;
    if let Some(expected) = request.expected_subtotal_paise
        && expected != price.subtotal
    {
        return Err(AppError::PriceChanged { expected_subtotal_paise: expected, actual_subtotal_paise: price.subtotal });
    }

    // 4. Effects. Rows are locked, so the stock guard cannot fail here; it
    //    and the CHECK (stock >= 0) constraint are defence in depth.
    let product_ids: Vec<Uuid> = lines.iter().map(|l| l.product_id).collect();
    let quantities: Vec<i32> = lines.iter().map(|l| l.quantity).collect();
    let decremented = sqlx::query(
        "UPDATE products AS p SET stock = p.stock - u.quantity, updated_at = now()
         FROM unnest($1::uuid[], $2::int[]) AS u(product_id, quantity)
         WHERE p.id = u.product_id AND p.stock >= u.quantity",
    )
    .bind(&product_ids)
    .bind(&quantities)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if decremented != lines.len() as u64 {
        return Err(AppError::Internal(format!("stock changed under lock for cart {cart_id}")));
    }

    let order_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO orders (id, cart_id, idempotency_key, request_fingerprint,
                             subtotal_paise, discount_paise, total_paise,
                             coupon_id, coupon_code, discount_percent)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(order_id)
    .bind(cart_id)
    .bind(idempotency_key)
    .bind(&fingerprint)
    .bind(price.subtotal)
    .bind(price.discount)
    .bind(price.total)
    .bind(coupon.as_ref().map(|c| c.id))
    .bind(coupon.as_ref().map(|c| &c.code))
    .bind(coupon.as_ref().map(|c| c.discount_percent))
    .execute(&mut *tx)
    .await
    .map_err(|err| match violated_constraint(&err) {
        // Same key used concurrently for a *different* cart (same-cart
        // duplicates were already serialized by the cart lock).
        Some("orders_idempotency_key_unique") => AppError::IdempotencyKeyReused,
        Some("orders_coupon_unique") => AppError::CouponAlreadyRedeemed(coupon_code.clone().unwrap_or_default()),
        Some("orders_cart_unique") => AppError::CartAlreadyCheckedOut(cart_id),
        _ => err.into(),
    })?;

    let line_totals = priced
        .iter()
        .map(|&(unit, qty)| money::line_total(unit, qty))
        .collect::<Result<Vec<_>, _>>()?;
    sqlx::query(
        "INSERT INTO order_items (order_id, product_id, product_name, unit_price_paise, quantity, line_total_paise)
         SELECT $1, u.* FROM unnest($2::uuid[], $3::text[], $4::bigint[], $5::int[], $6::bigint[]) AS u",
    )
    .bind(order_id)
    .bind(&product_ids)
    .bind(lines.iter().map(|l| l.name.clone()).collect::<Vec<_>>())
    .bind(lines.iter().map(|l| l.price_paise).collect::<Vec<_>>())
    .bind(&quantities)
    .bind(&line_totals)
    .execute(&mut *tx)
    .await?;

    sqlx::query("UPDATE carts SET status = 'checked_out', updated_at = now() WHERE id = $1")
        .bind(cart_id)
        .execute(&mut *tx)
        .await?;

    let order = orders::load(&mut tx, order_id).await?;
    tx.commit().await?;
    Ok(CheckoutOutcome::Created(order))
}

/// Locks the coupon row as a mutex for the rest of the transaction. A second
/// checkout using the same code blocks here until the first one commits or
/// rolls back; it then re-checks redemption against committed orders. If the
/// first checkout failed, nothing references the coupon and it is still usable.
async fn lock_unredeemed_coupon(conn: &mut PgConnection, code: &str) -> AppResult<LockedCoupon> {
    let coupon: LockedCoupon =
        sqlx::query_as("SELECT id, code, discount_percent FROM coupons WHERE code = $1 FOR UPDATE")
            .bind(code)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| AppError::UnknownCoupon(code.to_string()))?;
    // New statement => new READ COMMITTED snapshot taken after the lock was
    // granted, so it sees an order committed by the previous lock holder.
    let redeemed: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM orders WHERE coupon_id = $1)")
        .bind(coupon.id)
        .fetch_one(&mut *conn)
        .await?;
    if redeemed {
        return Err(AppError::CouponAlreadyRedeemed(coupon.code));
    }
    Ok(coupon)
}

async fn order_for_key(conn: &mut PgConnection, key: &str) -> AppResult<Option<OrderIdentity>> {
    Ok(sqlx::query_as("SELECT id, idempotency_key, request_fingerprint FROM orders WHERE idempotency_key = $1")
        .bind(key)
        .fetch_optional(&mut *conn)
        .await?)
}

async fn order_for_cart(conn: &mut PgConnection, cart_id: Uuid) -> AppResult<Option<OrderIdentity>> {
    Ok(sqlx::query_as("SELECT id, idempotency_key, request_fingerprint FROM orders WHERE cart_id = $1")
        .bind(cart_id)
        .fetch_optional(&mut *conn)
        .await?)
}

async fn replay(conn: &mut PgConnection, existing: OrderIdentity, fingerprint: &str) -> AppResult<CheckoutOutcome> {
    if existing.request_fingerprint != fingerprint {
        return Err(AppError::IdempotencyKeyReused);
    }
    Ok(CheckoutOutcome::Replayed(orders::load(conn, existing.id).await?))
}

fn normalize_coupon_code(raw: &str) -> AppResult<String> {
    let code = raw.trim().to_ascii_uppercase();
    if code.is_empty() || code.len() > MAX_COUPON_CODE_LEN {
        return Err(AppError::Validation {
            field: "coupon_code",
            message: format!("coupon_code must be 1-{MAX_COUPON_CODE_LEN} characters, or omitted"),
        });
    }
    Ok(code)
}

/// Identifies "the same checkout request" for idempotency purposes. Uses the
/// normalized coupon code so `save10` and ` SAVE10 ` are the same request.
fn fingerprint(cart_id: Uuid, coupon_code: Option<&str>, expected_subtotal: Option<Paise>) -> String {
    let canonical = format!(
        "checkout/v1|cart={cart_id}|coupon={}|expected_subtotal={}",
        coupon_code.unwrap_or(""),
        expected_subtotal.map(|v| v.to_string()).unwrap_or_default(),
    );
    format!("{:x}", Sha256::digest(canonical.as_bytes()))
}
