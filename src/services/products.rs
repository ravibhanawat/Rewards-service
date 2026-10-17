use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult, Shortfall},
    models::Product,
};

/// ₹1,000,000,000.00 — far above any real price, low enough that a full cart
/// cannot overflow i64 paise.
pub const MAX_UNIT_PRICE_PAISE: i64 = 100_000_000_000;

pub async fn list(pool: &PgPool) -> AppResult<Vec<Product>> {
    Ok(sqlx::query_as("SELECT * FROM products ORDER BY name, id")
        .fetch_all(pool)
        .await?)
}

pub async fn get(pool: &PgPool, id: Uuid) -> AppResult<Product> {
    sqlx::query_as("SELECT * FROM products WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or(AppError::ProductNotFound(id))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductUpdate {
    pub name: Option<String>,
    pub price_paise: Option<i64>,
    /// Relative adjustment (restock or write-off). An absolute "set stock to N"
    /// would silently overwrite decrements from checkouts that commit between
    /// the admin reading the stock and writing it back.
    pub stock_delta: Option<i32>,
    pub active: Option<bool>,
}

/// Administrative product edit, used to exercise price and availability
/// changes between "add to cart" and checkout.
pub async fn update(pool: &PgPool, id: Uuid, update: ProductUpdate) -> AppResult<Product> {
    if update.name.is_none()
        && update.price_paise.is_none()
        && update.stock_delta.is_none()
        && update.active.is_none()
    {
        return Err(AppError::Validation {
            field: "body",
            message: "provide at least one field to update".into(),
        });
    }
    if let Some(name) = &update.name
        && !(1..=200).contains(&name.trim().chars().count())
    {
        return Err(AppError::Validation {
            field: "name",
            message: "name must be 1-200 characters".into(),
        });
    }
    if let Some(price) = update.price_paise
        && !(0..=MAX_UNIT_PRICE_PAISE).contains(&price)
    {
        return Err(AppError::Validation {
            field: "price_paise",
            message: format!("price_paise must be between 0 and {MAX_UNIT_PRICE_PAISE}"),
        });
    }
    let delta = update.stock_delta.unwrap_or(0);

    // Row lock via UPDATE: waits for any checkout currently holding this
    // product, then applies the delta to the committed stock.
    let updated: Option<Product> = sqlx::query_as(
        "UPDATE products SET
             name = COALESCE($2, name),
             price_paise = COALESCE($3, price_paise),
             stock = stock + $4,
             active = COALESCE($5, active),
             updated_at = now()
         WHERE id = $1 AND stock + $4 >= 0
         RETURNING *",
    )
    .bind(id)
    .bind(update.name.as_deref().map(str::trim))
    .bind(update.price_paise)
    .bind(delta)
    .bind(update.active)
    .fetch_optional(pool)
    .await?;
    if let Some(product) = updated {
        return Ok(product);
    }
    let stock: Option<i32> = sqlx::query_scalar("SELECT stock FROM products WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    match stock {
        None => Err(AppError::ProductNotFound(id)),
        Some(available) => Err(AppError::InsufficientInventory(vec![Shortfall {
            product_id: id,
            requested: delta.saturating_neg(),
            available,
        }])),
    }
}
