//! One error type for the whole service and its single HTTP mapping.
//!
//! Status code policy:
//! - 400: the request is malformed (bad JSON, bad path, missing/invalid header).
//! - 404: the resource addressed by the URL does not exist.
//! - 409: the request is valid but conflicts with current server state
//!   (cart already checked out, stock, coupon already used, ...).
//!   Retrying may succeed once that state changes.
//! - 422: the request is well-formed but its content is invalid on its own
//!   (quantity out of range, unknown product/coupon, reused idempotency key).
//! - 500: unexpected; details are logged, never returned.
//!
//! Clients should branch on `error.code`; the status is only the category.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::money::{Overflow, Paise};

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Shortfall {
    pub product_id: Uuid,
    pub requested: i32,
    pub available: i32,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error("the Idempotency-Key header is required for checkout")]
    MissingIdempotencyKey,
    #[error("the Idempotency-Key header must be 1-255 visible ASCII characters")]
    InvalidIdempotencyKey,

    #[error("{message}")]
    Validation {
        field: &'static str,
        message: String,
    },
    #[error("product {0} does not exist")]
    UnknownProduct(Uuid),
    #[error("coupon {0:?} does not exist")]
    UnknownCoupon(String),
    #[error("this Idempotency-Key was already used for a different checkout request")]
    IdempotencyKeyReused,

    #[error("cart {0} not found")]
    CartNotFound(Uuid),
    #[error("product {0} not found")]
    ProductNotFound(Uuid),
    #[error("order {0} not found")]
    OrderNotFound(Uuid),
    #[error("no such route")]
    RouteNotFound,
    #[error("product {product_id} is not in cart {cart_id}")]
    ItemNotInCart { cart_id: Uuid, product_id: Uuid },

    #[error("cart {0} has already been checked out")]
    CartAlreadyCheckedOut(Uuid),
    #[error("product {product_id} is already in the cart; use PATCH to change its quantity")]
    ItemAlreadyInCart { product_id: Uuid },
    #[error("cart is empty")]
    EmptyCart,
    #[error("requested quantity exceeds available stock")]
    InsufficientInventory(Vec<Shortfall>),
    #[error("one or more products are no longer available")]
    ProductUnavailable(Vec<Uuid>),
    #[error("coupon {0:?} has already been redeemed")]
    CouponAlreadyRedeemed(String),
    #[error("cart subtotal changed since it was last viewed")]
    PriceChanged {
        expected_subtotal_paise: Paise,
        actual_subtotal_paise: Paise,
    },
    #[error("no unrewarded order milestone is eligible for a coupon")]
    NoEligibleMilestone {
        successful_orders: i64,
        every_n_orders: i64,
        next_milestone_order_count: i64,
    },
    #[error("the request conflicted with a concurrent operation; it is safe to retry")]
    Contention,
    #[error("the service is temporarily overloaded; retry shortly")]
    Unavailable,

    #[error("internal error")]
    Internal(String),
}

impl AppError {
    fn status_and_code(&self) -> (StatusCode, &'static str) {
        use AppError::*;
        match self {
            InvalidRequest(_) => (StatusCode::BAD_REQUEST, "INVALID_REQUEST"),
            MissingIdempotencyKey => (StatusCode::BAD_REQUEST, "IDEMPOTENCY_KEY_MISSING"),
            InvalidIdempotencyKey => (StatusCode::BAD_REQUEST, "IDEMPOTENCY_KEY_INVALID"),
            Validation { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION_FAILED"),
            UnknownProduct(_) => (StatusCode::UNPROCESSABLE_ENTITY, "UNKNOWN_PRODUCT"),
            UnknownCoupon(_) => (StatusCode::UNPROCESSABLE_ENTITY, "UNKNOWN_COUPON"),
            IdempotencyKeyReused => (StatusCode::UNPROCESSABLE_ENTITY, "IDEMPOTENCY_KEY_REUSED"),
            CartNotFound(_) => (StatusCode::NOT_FOUND, "CART_NOT_FOUND"),
            ProductNotFound(_) => (StatusCode::NOT_FOUND, "PRODUCT_NOT_FOUND"),
            OrderNotFound(_) => (StatusCode::NOT_FOUND, "ORDER_NOT_FOUND"),
            RouteNotFound => (StatusCode::NOT_FOUND, "ROUTE_NOT_FOUND"),
            ItemNotInCart { .. } => (StatusCode::NOT_FOUND, "ITEM_NOT_IN_CART"),
            CartAlreadyCheckedOut(_) => (StatusCode::CONFLICT, "CART_ALREADY_CHECKED_OUT"),
            ItemAlreadyInCart { .. } => (StatusCode::CONFLICT, "ITEM_ALREADY_IN_CART"),
            EmptyCart => (StatusCode::CONFLICT, "EMPTY_CART"),
            InsufficientInventory(_) => (StatusCode::CONFLICT, "INSUFFICIENT_INVENTORY"),
            ProductUnavailable(_) => (StatusCode::CONFLICT, "PRODUCT_UNAVAILABLE"),
            CouponAlreadyRedeemed(_) => (StatusCode::CONFLICT, "COUPON_ALREADY_REDEEMED"),
            PriceChanged { .. } => (StatusCode::CONFLICT, "PRICE_CHANGED"),
            NoEligibleMilestone { .. } => (StatusCode::CONFLICT, "NO_ELIGIBLE_MILESTONE"),
            Contention => (StatusCode::CONFLICT, "CONCURRENT_UPDATE"),
            Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE"),
            Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR"),
        }
    }

    fn details(&self) -> Option<Value> {
        use AppError::*;
        let details = match self {
            Validation { field, .. } => json!({ "field": field }),
            UnknownProduct(id) | ProductNotFound(id) => json!({ "product_id": id }),
            UnknownCoupon(code) | CouponAlreadyRedeemed(code) => json!({ "coupon_code": code }),
            CartNotFound(id) | CartAlreadyCheckedOut(id) => json!({ "cart_id": id }),
            OrderNotFound(id) => json!({ "order_id": id }),
            ItemNotInCart {
                cart_id,
                product_id,
            } => json!({ "cart_id": cart_id, "product_id": product_id }),
            ItemAlreadyInCart { product_id } => json!({ "product_id": product_id }),
            InsufficientInventory(items) => json!({ "items": items }),
            ProductUnavailable(ids) => json!({ "product_ids": ids }),
            PriceChanged {
                expected_subtotal_paise,
                actual_subtotal_paise,
            } => json!({
                "expected_subtotal_paise": expected_subtotal_paise,
                "actual_subtotal_paise": actual_subtotal_paise,
            }),
            NoEligibleMilestone {
                successful_orders,
                every_n_orders,
                next_milestone_order_count,
            } => json!({
                "successful_orders": successful_orders,
                "every_n_orders": every_n_orders,
                "next_milestone_order_count": next_milestone_order_count,
                "orders_until_next_milestone": next_milestone_order_count - successful_orders,
            }),
            Contention | Unavailable => json!({ "retryable": true }),
            _ => return None,
        };
        Some(details)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        if let AppError::Internal(cause) = &self {
            tracing::error!(%cause, "internal error");
        }
        let mut error = json!({ "code": code, "message": self.to_string() });
        if let Some(details) = self.details() {
            error["details"] = details;
        }
        (status, axum::Json(json!({ "error": error }))).into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        if matches!(err, sqlx::Error::PoolTimedOut) {
            return AppError::Unavailable;
        }
        if let sqlx::Error::Database(db) = &err {
            // 40001 serialization_failure, 40P01 deadlock_detected,
            // 55P03 lock_not_available (lock_timeout). None of these leave
            // partial effects: the transaction is rolled back.
            if matches!(db.code().as_deref(), Some("40001" | "40P01" | "55P03")) {
                return AppError::Contention;
            }
        }
        AppError::Internal(err.to_string())
    }
}

impl From<Overflow> for AppError {
    fn from(_: Overflow) -> Self {
        AppError::Validation {
            field: "cart",
            message: "cart total exceeds the supported amount".into(),
        }
    }
}

/// Name of the constraint a database error violated, if any.
pub fn violated_constraint(err: &sqlx::Error) -> Option<&str> {
    match err {
        sqlx::Error::Database(db) => db.constraint(),
        _ => None,
    }
}
