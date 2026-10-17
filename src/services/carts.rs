use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult, Shortfall},
    models::{CURRENCY, CartLine, CartView},
    money,
};

pub const MAX_LINE_QUANTITY: i64 = 1000;

pub async fn create(pool: &PgPool) -> AppResult<CartView> {
    let mut conn = pool.acquire().await?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO carts (id) VALUES ($1)")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    load(&mut conn, id).await
}

pub async fn get(pool: &PgPool, cart_id: Uuid) -> AppResult<CartView> {
    load(&mut *pool.acquire().await?, cart_id).await
}

/// Adds a new line. Adding a product that is already in the cart is a 409
/// rather than an increment, so a retried request cannot double the quantity.
pub async fn add_item(
    pool: &PgPool,
    cart_id: Uuid,
    product_id: Uuid,
    quantity: i64,
) -> AppResult<CartView> {
    let quantity = validate_quantity(quantity)?;
    let mut tx = pool.begin().await?;
    lock_open_cart(&mut tx, cart_id).await?;
    check_purchasable(&mut tx, product_id, quantity).await?;
    let inserted = sqlx::query(
        "INSERT INTO cart_items (cart_id, product_id, quantity) VALUES ($1, $2, $3)
         ON CONFLICT (cart_id, product_id) DO NOTHING",
    )
    .bind(cart_id)
    .bind(product_id)
    .bind(quantity)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Err(AppError::ItemAlreadyInCart { product_id });
    }
    let cart = load(&mut tx, cart_id).await?;
    tx.commit().await?;
    Ok(cart)
}

/// Sets the absolute quantity of an existing line (idempotent).
pub async fn update_item(
    pool: &PgPool,
    cart_id: Uuid,
    product_id: Uuid,
    quantity: i64,
) -> AppResult<CartView> {
    let quantity = validate_quantity(quantity)?;
    let mut tx = pool.begin().await?;
    lock_open_cart(&mut tx, cart_id).await?;
    let updated = sqlx::query(
        "UPDATE cart_items SET quantity = $3, updated_at = now() WHERE cart_id = $1 AND product_id = $2",
    )
    .bind(cart_id)
    .bind(product_id)
    .bind(quantity)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(AppError::ItemNotInCart {
            cart_id,
            product_id,
        });
    }
    check_purchasable(&mut tx, product_id, quantity).await?;
    let cart = load(&mut tx, cart_id).await?;
    tx.commit().await?;
    Ok(cart)
}

pub async fn remove_item(pool: &PgPool, cart_id: Uuid, product_id: Uuid) -> AppResult<CartView> {
    let mut tx = pool.begin().await?;
    lock_open_cart(&mut tx, cart_id).await?;
    let deleted = sqlx::query("DELETE FROM cart_items WHERE cart_id = $1 AND product_id = $2")
        .bind(cart_id)
        .bind(product_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(AppError::ItemNotInCart {
            cart_id,
            product_id,
        });
    }
    let cart = load(&mut tx, cart_id).await?;
    tx.commit().await?;
    Ok(cart)
}

fn validate_quantity(quantity: i64) -> AppResult<i32> {
    if (1..=MAX_LINE_QUANTITY).contains(&quantity) {
        Ok(quantity as i32)
    } else {
        Err(AppError::Validation {
            field: "quantity",
            message: format!("quantity must be an integer between 1 and {MAX_LINE_QUANTITY}"),
        })
    }
}

/// Locks the cart row until the transaction ends and fails unless the cart is
/// open. Checkout takes the same row lock, so cart contents cannot change
/// while a checkout is reading them, and nothing can be added afterwards.
async fn lock_open_cart(conn: &mut PgConnection, cart_id: Uuid) -> AppResult<()> {
    let locked =
        sqlx::query("UPDATE carts SET updated_at = now() WHERE id = $1 AND status = 'open'")
            .bind(cart_id)
            .execute(&mut *conn)
            .await?
            .rows_affected();
    if locked == 1 {
        return Ok(());
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM carts WHERE id = $1)")
        .bind(cart_id)
        .fetch_one(&mut *conn)
        .await?;
    Err(if exists {
        AppError::CartAlreadyCheckedOut(cart_id)
    } else {
        AppError::CartNotFound(cart_id)
    })
}

/// Advisory check at add/update time so obviously invalid lines never enter a
/// cart. It does not reserve stock: checkout re-validates under row locks.
async fn check_purchasable(
    conn: &mut PgConnection,
    product_id: Uuid,
    quantity: i32,
) -> AppResult<()> {
    let product: Option<(i32, bool)> =
        sqlx::query_as("SELECT stock, active FROM products WHERE id = $1")
            .bind(product_id)
            .fetch_optional(&mut *conn)
            .await?;
    match product {
        None => Err(AppError::UnknownProduct(product_id)),
        Some((_, false)) => Err(AppError::ProductUnavailable(vec![product_id])),
        Some((stock, true)) if quantity > stock => {
            Err(AppError::InsufficientInventory(vec![Shortfall {
                product_id,
                requested: quantity,
                available: stock,
            }]))
        }
        Some(_) => Ok(()),
    }
}

#[derive(FromRow)]
struct CartRow {
    id: Uuid,
    status: String,
    order_id: Option<Uuid>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(FromRow)]
struct LineRow {
    product_id: Uuid,
    name: String,
    price_paise: i64,
    quantity: i32,
    stock: i32,
    active: bool,
}

async fn load(conn: &mut PgConnection, cart_id: Uuid) -> AppResult<CartView> {
    let cart: CartRow = sqlx::query_as(
        "SELECT c.id, c.status, o.id AS order_id, c.created_at, c.updated_at
         FROM carts c LEFT JOIN orders o ON o.cart_id = c.id
         WHERE c.id = $1",
    )
    .bind(cart_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(AppError::CartNotFound(cart_id))?;

    let rows: Vec<LineRow> = sqlx::query_as(
        "SELECT ci.product_id, p.name, p.price_paise, ci.quantity, p.stock, p.active
         FROM cart_items ci JOIN products p ON p.id = ci.product_id
         WHERE ci.cart_id = $1
         ORDER BY ci.created_at, ci.product_id",
    )
    .bind(cart_id)
    .fetch_all(&mut *conn)
    .await?;

    let priced: Vec<_> = rows.iter().map(|r| (r.price_paise, r.quantity)).collect();
    let subtotal = money::price_order(&priced, None)?.subtotal;
    let items = rows
        .into_iter()
        .map(|r| {
            Ok(CartLine {
                line_total_paise: money::line_total(r.price_paise, r.quantity)?,
                purchasable: r.active && r.quantity <= r.stock,
                product_id: r.product_id,
                name: r.name,
                unit_price_paise: r.price_paise,
                quantity: r.quantity,
                stock_available: r.stock,
            })
        })
        .collect::<AppResult<Vec<_>>>()?;

    Ok(CartView {
        id: cart.id,
        status: cart.status,
        currency: CURRENCY,
        total_quantity: items.iter().map(|i| i64::from(i.quantity)).sum(),
        items,
        subtotal_paise: subtotal,
        order_id: cart.order_id,
        created_at: cart.created_at,
        updated_at: cart.updated_at,
    })
}
