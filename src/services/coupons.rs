//! Reward coupons. Milestone k is reached once k * n orders have been placed
//! successfully; each milestone can be rewarded with exactly one coupon.

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    config::RewardPolicy,
    error::{AppError, AppResult, violated_constraint},
    models::{CouponView, GeneratedCoupon},
};

/// Arbitrary, fixed key for `pg_advisory_xact_lock`: ASCII "coupon".
const GENERATION_LOCK_KEY: i64 = 0x636f_7570_6f6e;

const COUPON_SELECT: &str = "
    SELECT c.id, c.code, c.milestone_order_count, c.discount_percent,
           CASE WHEN o.id IS NULL THEN 'available' ELSE 'redeemed' END AS status,
           o.id AS redeemed_order_id, o.created_at AS redeemed_at, c.created_at
    FROM coupons c LEFT JOIN orders o ON o.coupon_id = c.id";

/// Generates the coupon for the earliest reached-but-unrewarded milestone.
/// At most one coupon per call; call again to reward further milestones.
pub async fn generate(pool: &PgPool, policy: RewardPolicy) -> AppResult<GeneratedCoupon> {
    let mut tx = pool.begin().await?;
    // Generators queue behind one transaction-scoped lock, across every app
    // instance sharing this database, so two administrators cannot both pick
    // the same milestone. coupons_milestone_unique backs this up.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(GENERATION_LOCK_KEY)
        .execute(&mut *tx)
        .await?;

    // Orders are append-only and only exist once committed, so this count is
    // exactly "successfully placed orders" and can only grow.
    let successful_orders: i64 = sqlx::query_scalar("SELECT count(*) FROM orders").fetch_one(&mut *tx).await?;
    let n = policy.every_n_orders;
    let unrewarded: Vec<i64> = sqlx::query_scalar(
        "SELECT m FROM generate_series($1::bigint, $2::bigint, $1::bigint) AS m
         WHERE NOT EXISTS (SELECT 1 FROM coupons c WHERE c.milestone_order_count = m)
         ORDER BY m",
    )
    .bind(n)
    .bind(successful_orders)
    .fetch_all(&mut *tx)
    .await?;

    let Some(&milestone) = unrewarded.first() else {
        return Err(AppError::NoEligibleMilestone {
            successful_orders,
            every_n_orders: n,
            next_milestone_order_count: (successful_orders / n + 1) * n,
        });
    };

    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO coupons (id, code, milestone_order_count, discount_percent) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(new_code())
        .bind(milestone)
        .bind(policy.discount_percent)
        .execute(&mut *tx)
        .await
        .map_err(|err| match violated_constraint(&err) {
            // Unreachable while the advisory lock is held; still never a 500.
            Some("coupons_milestone_unique" | "coupons_code_unique") => AppError::Contention,
            _ => err.into(),
        })?;
    let coupon = load(&mut tx, id).await?;
    tx.commit().await?;

    Ok(GeneratedCoupon { coupon, successful_orders, remaining_eligible_milestones: unrewarded.len() as i64 - 1 })
}

pub async fn list(pool: &PgPool) -> AppResult<Vec<CouponView>> {
    Ok(sqlx::query_as(&format!("{COUPON_SELECT} ORDER BY c.milestone_order_count")).fetch_all(pool).await?)
}

async fn load(conn: &mut PgConnection, id: Uuid) -> AppResult<CouponView> {
    Ok(sqlx::query_as(&format!("{COUPON_SELECT} WHERE c.id = $1")).bind(id).fetch_one(&mut *conn).await?)
}

/// Codes are bearer tokens (anyone holding one can redeem it), so they are
/// random rather than derived from the milestone: 40 random bits.
fn new_code() -> String {
    let random = Uuid::new_v4().simple().to_string().to_ascii_uppercase();
    format!("RWD-{}", &random[..10])
}
