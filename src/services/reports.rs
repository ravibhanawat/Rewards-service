use sqlx::PgPool;

use crate::{
    error::AppResult,
    models::{CURRENCY, CouponCounts, ProductSales, SummaryReport},
};

/// Read-only summary derived from committed orders, order items and coupons.
///
/// The three aggregates are separate queries (joining orders to items and
/// coupons in one query would multiply order totals by item count) but share
/// one REPEATABLE READ snapshot, so they reconcile with each other even while
/// checkouts commit concurrently. READ ONLY guarantees no mutation.
pub async fn summary(pool: &PgPool) -> AppResult<SummaryReport> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;

    let (successful_orders, gross, discounts, net): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*),
                COALESCE(sum(subtotal_paise), 0)::bigint,
                COALESCE(sum(discount_paise), 0)::bigint,
                COALESCE(sum(total_paise), 0)::bigint
         FROM orders",
    )
    .fetch_one(&mut *tx)
    .await?;

    let coupons: CouponCounts = sqlx::query_as(
        "SELECT count(*) AS generated,
                count(*) - count(o.id) AS available,
                count(o.id) AS redeemed
         FROM coupons c LEFT JOIN orders o ON o.coupon_id = c.id",
    )
    .fetch_one(&mut *tx)
    .await?;

    let products: Vec<ProductSales> = sqlx::query_as(
        "SELECT p.id AS product_id, p.name,
                COALESCE(sum(oi.quantity), 0)::bigint AS quantity_sold,
                COALESCE(sum(oi.line_total_paise), 0)::bigint AS gross_revenue_paise
         FROM products p LEFT JOIN order_items oi ON oi.product_id = p.id
         GROUP BY p.id
         ORDER BY p.name, p.id",
    )
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(SummaryReport {
        currency: CURRENCY,
        successful_orders,
        gross_revenue_paise: gross,
        total_discounts_paise: discounts,
        net_revenue_paise: net,
        coupons,
        products,
    })
}
