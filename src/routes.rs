//! HTTP handlers: translate requests into service calls and outcomes into
//! status codes. No business rules live here.
//!
//! Routes under `/admin` are administrative. Authentication is out of scope
//! for this exercise; in production they would sit behind admin auth.

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header::LOCATION},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use serde::Deserialize;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::{
    AppState,
    error::{AppError, AppResult},
    extract::{Json, Path},
    models::{CartView, CouponView, GeneratedCoupon, OrderView, Product, SummaryReport},
    services::{
        carts,
        checkout::{self, CheckoutOutcome, CheckoutRequest, MAX_IDEMPOTENCY_KEY_LEN},
        coupons, orders,
        products::{self, ProductUpdate},
        reports,
    },
};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/products", get(list_products))
        .route("/products/{product_id}", get(get_product))
        .route("/carts", post(create_cart))
        .route("/carts/{cart_id}", get(get_cart))
        .route("/carts/{cart_id}/items", post(add_item))
        .route(
            "/carts/{cart_id}/items/{product_id}",
            patch(update_item).delete(remove_item),
        )
        .route("/carts/{cart_id}/checkout", post(checkout))
        .route("/orders/{order_id}", get(get_order))
        .route("/admin/products/{product_id}", patch(update_product))
        .route("/admin/orders", get(list_orders))
        .route("/admin/coupons", get(list_coupons))
        .route("/admin/coupons/generate", post(generate_coupon))
        .route("/admin/reports/summary", get(summary_report))
        .fallback(|| async { AppError::RouteNotFound })
        .method_not_allowed_fallback(|| async { AppError::MethodNotAllowed })
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn list_products(State(state): State<AppState>) -> AppResult<Json<Vec<Product>>> {
    Ok(Json(products::list(&state.pool).await?))
}

async fn get_product(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Product>> {
    Ok(Json(products::get(&state.pool, id).await?))
}

async fn create_cart(State(state): State<AppState>) -> AppResult<impl IntoResponse> {
    let cart = carts::create(&state.pool).await?;
    Ok((
        StatusCode::CREATED,
        [(LOCATION, format!("/carts/{}", cart.id))],
        Json(cart),
    ))
}

async fn get_cart(
    State(state): State<AppState>,
    Path(cart_id): Path<Uuid>,
) -> AppResult<Json<CartView>> {
    Ok(Json(carts::get(&state.pool, cart_id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddItem {
    product_id: Uuid,
    /// i64 so that negative or huge values reach domain validation (422)
    /// instead of failing deserialization (400).
    quantity: i64,
}

async fn add_item(
    State(state): State<AppState>,
    Path(cart_id): Path<Uuid>,
    Json(body): Json<AddItem>,
) -> AppResult<(StatusCode, Json<CartView>)> {
    let cart = carts::add_item(&state.pool, cart_id, body.product_id, body.quantity).await?;
    Ok((StatusCode::CREATED, Json(cart)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateItem {
    quantity: i64,
}

async fn update_item(
    State(state): State<AppState>,
    Path((cart_id, product_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<UpdateItem>,
) -> AppResult<Json<CartView>> {
    Ok(Json(
        carts::update_item(&state.pool, cart_id, product_id, body.quantity).await?,
    ))
}

async fn remove_item(
    State(state): State<AppState>,
    Path((cart_id, product_id)): Path<(Uuid, Uuid)>,
) -> AppResult<Json<CartView>> {
    Ok(Json(
        carts::remove_item(&state.pool, cart_id, product_id).await?,
    ))
}

/// 201 + Location for a new order; 200 + `Idempotent-Replayed: true` when the
/// same key and payload already produced an order.
async fn checkout(
    State(state): State<AppState>,
    Path(cart_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<Response> {
    let key = idempotency_key(&headers)?;
    // The body is optional (no coupon, no price guard), so parse it by hand
    // rather than demanding a JSON content type.
    let request: CheckoutRequest = if body.iter().all(u8::is_ascii_whitespace) {
        CheckoutRequest::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| AppError::InvalidRequest(format!("invalid checkout body: {e}")))?
    };
    Ok(
        match checkout::checkout(&state.pool, cart_id, key, request).await? {
            CheckoutOutcome::Created(order) => (
                StatusCode::CREATED,
                [(LOCATION, format!("/orders/{}", order.id))],
                Json(order),
            )
                .into_response(),
            CheckoutOutcome::Replayed(order) => (
                StatusCode::OK,
                [("idempotent-replayed", "true")],
                Json(order),
            )
                .into_response(),
        },
    )
}

fn idempotency_key(headers: &HeaderMap) -> AppResult<&str> {
    let value = headers
        .get("idempotency-key")
        .ok_or(AppError::MissingIdempotencyKey)?;
    let key = value
        .to_str()
        .map_err(|_| AppError::InvalidIdempotencyKey)?;
    if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_LEN {
        return Err(AppError::InvalidIdempotencyKey);
    }
    Ok(key)
}

async fn get_order(
    State(state): State<AppState>,
    Path(order_id): Path<Uuid>,
) -> AppResult<Json<OrderView>> {
    Ok(Json(orders::get(&state.pool, order_id).await?))
}

async fn update_product(
    State(state): State<AppState>,
    Path(product_id): Path<Uuid>,
    Json(body): Json<ProductUpdate>,
) -> AppResult<Json<Product>> {
    Ok(Json(products::update(&state.pool, product_id, body).await?))
}

async fn list_orders(State(state): State<AppState>) -> AppResult<Json<Vec<OrderView>>> {
    Ok(Json(orders::list(&state.pool).await?))
}

async fn list_coupons(State(state): State<AppState>) -> AppResult<Json<Vec<CouponView>>> {
    Ok(Json(coupons::list(&state.pool).await?))
}

async fn generate_coupon(
    State(state): State<AppState>,
) -> AppResult<(StatusCode, Json<GeneratedCoupon>)> {
    Ok((
        StatusCode::CREATED,
        Json(coupons::generate(&state.pool, state.rewards).await?),
    ))
}

async fn summary_report(State(state): State<AppState>) -> AppResult<Json<SummaryReport>> {
    Ok(Json(reports::summary(&state.pool).await?))
}
