use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    models::{AppliedCoupon, CURRENCY, OrderItem, OrderView},
};

const ORDER_COLUMNS: &str = "id, cart_id, subtotal_paise, discount_paise, total_paise, coupon_code, discount_percent, created_at";

#[derive(FromRow)]
struct OrderRow {
    id: Uuid,
    cart_id: Uuid,
    subtotal_paise: i64,
    discount_paise: i64,
    total_paise: i64,
    coupon_code: Option<String>,
    discount_percent: Option<i32>,
    created_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct ItemRow {
    order_id: Uuid,
    #[sqlx(flatten)]
    item: OrderItem,
}

impl OrderRow {
    fn into_view(self, items: Vec<OrderItem>) -> OrderView {
        let coupon = match (self.coupon_code, self.discount_percent) {
            (Some(code), Some(discount_percent)) => Some(AppliedCoupon {
                code,
                discount_percent,
            }),
            _ => None,
        };
        OrderView {
            id: self.id,
            cart_id: self.cart_id,
            status: "placed",
            currency: CURRENCY,
            items,
            subtotal_paise: self.subtotal_paise,
            coupon,
            discount_paise: self.discount_paise,
            total_paise: self.total_paise,
            created_at: self.created_at,
        }
    }
}

pub async fn get(pool: &PgPool, id: Uuid) -> AppResult<OrderView> {
    load(&mut *pool.acquire().await?, id).await
}

pub async fn load(conn: &mut PgConnection, id: Uuid) -> AppResult<OrderView> {
    let order: OrderRow =
        sqlx::query_as(&format!("SELECT {ORDER_COLUMNS} FROM orders WHERE id = $1"))
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or(AppError::OrderNotFound(id))?;
    let items: Vec<OrderItem> = sqlx::query_as(
        "SELECT product_id, product_name, unit_price_paise, quantity, line_total_paise
         FROM order_items WHERE order_id = $1 ORDER BY product_id",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(order.into_view(items))
}

/// Every order, oldest first (administrative; unpaginated for this exercise).
pub async fn list(pool: &PgPool) -> AppResult<Vec<OrderView>> {
    // One snapshot for both queries so every listed order has all its items.
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let orders: Vec<OrderRow> = sqlx::query_as(&format!(
        "SELECT {ORDER_COLUMNS} FROM orders ORDER BY created_at, id"
    ))
    .fetch_all(&mut *tx)
    .await?;
    let items: Vec<ItemRow> = sqlx::query_as(
        "SELECT order_id, product_id, product_name, unit_price_paise, quantity, line_total_paise
         FROM order_items ORDER BY order_id, product_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let mut by_order: HashMap<Uuid, Vec<OrderItem>> = HashMap::new();
    for row in items {
        by_order.entry(row.order_id).or_default().push(row.item);
    }
    Ok(orders
        .into_iter()
        .map(|o| {
            let items = by_order.remove(&o.id).unwrap_or_default();
            o.into_view(items)
        })
        .collect())
}
