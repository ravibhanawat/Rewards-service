//! Rows read from the database and the JSON shapes returned by the API.
//! All money fields are integer paise and are suffixed `_paise`.

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::money::Paise;

pub const CURRENCY: &str = "INR";

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Product {
    pub id: Uuid,
    pub name: String,
    pub price_paise: Paise,
    pub stock: i32,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct CartView {
    pub id: Uuid,
    /// `open` or `checked_out`.
    pub status: String,
    pub currency: &'static str,
    pub items: Vec<CartLine>,
    pub total_quantity: i64,
    /// Indicative: computed from *current* product prices. The price actually
    /// charged is fixed at checkout and recorded on the order.
    pub subtotal_paise: Paise,
    /// Set once the cart has been checked out.
    pub order_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CartLine {
    pub product_id: Uuid,
    pub name: String,
    pub unit_price_paise: Paise,
    pub quantity: i32,
    pub line_total_paise: Paise,
    pub stock_available: i32,
    /// False if the product was discontinued or stock fell below the quantity
    /// since it was added. Checkout would currently fail for this line.
    pub purchasable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderView {
    pub id: Uuid,
    pub cart_id: Uuid,
    pub status: &'static str,
    pub currency: &'static str,
    pub items: Vec<OrderItem>,
    /// Sum of `line_total_paise`, before discount.
    pub subtotal_paise: Paise,
    pub coupon: Option<AppliedCoupon>,
    pub discount_paise: Paise,
    /// `subtotal_paise - discount_paise`; what the customer paid.
    pub total_paise: Paise,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct OrderItem {
    pub product_id: Uuid,
    /// Name and price as they were at purchase time.
    pub product_name: String,
    pub unit_price_paise: Paise,
    pub quantity: i32,
    pub line_total_paise: Paise,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppliedCoupon {
    pub code: String,
    pub discount_percent: i32,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CouponView {
    pub id: Uuid,
    pub code: String,
    pub milestone_order_count: i64,
    pub discount_percent: i32,
    /// `available` or `redeemed`, derived from whether an order references it.
    pub status: String,
    pub redeemed_order_id: Option<Uuid>,
    pub redeemed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct GeneratedCoupon {
    pub coupon: CouponView,
    pub successful_orders: i64,
    /// Further milestones that are already reached but not yet rewarded.
    pub remaining_eligible_milestones: i64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SummaryReport {
    pub currency: &'static str,
    pub successful_orders: i64,
    pub gross_revenue_paise: Paise,
    pub total_discounts_paise: Paise,
    pub net_revenue_paise: Paise,
    pub coupons: CouponCounts,
    pub products: Vec<ProductSales>,
}

#[derive(Debug, Serialize, PartialEq, Eq, sqlx::FromRow)]
pub struct CouponCounts {
    pub generated: i64,
    pub available: i64,
    pub redeemed: i64,
}

#[derive(Debug, Serialize, PartialEq, Eq, sqlx::FromRow)]
pub struct ProductSales {
    pub product_id: Uuid,
    pub name: String,
    pub quantity_sold: i64,
    pub gross_revenue_paise: Paise,
}
