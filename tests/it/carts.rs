use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::spawn_app;

#[tokio::test]
async fn new_cart_is_open_and_empty() {
    let app = spawn_app().await;

    let created = app.post("/carts", json!({})).await;
    assert_eq!(created.status, 201);
    let cart = created.id();
    assert_eq!(
        created.header("location"),
        Some(format!("/carts/{cart}").as_str())
    );

    let r = app.get(&format!("/carts/{cart}")).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body["status"], "open");
    assert_eq!(r.body["items"], json!([]));
    assert_eq!(r.body["subtotal_paise"], 0);
    assert_eq!(r.body["currency"], "INR");
    assert_eq!(r.body["order_id"], Value::Null);

    // Missing vs malformed identifiers are distinguishable.
    let missing = app.get(&format!("/carts/{}", Uuid::new_v4())).await;
    assert_eq!(
        (missing.status.as_u16(), missing.code()),
        (404, "CART_NOT_FOUND")
    );
    let malformed = app.get("/carts/not-a-uuid").await;
    assert_eq!(
        (malformed.status.as_u16(), malformed.code()),
        (400, "INVALID_REQUEST")
    );
}

#[tokio::test]
async fn unknown_routes_and_methods_use_the_error_envelope() {
    let app = spawn_app().await;
    let r = app.get("/no/such/route").await;
    assert_eq!((r.status.as_u16(), r.code()), (404, "ROUTE_NOT_FOUND"));
    let r = app.request(Method::PUT, "/carts", None, &[]).await;
    assert_eq!((r.status.as_u16(), r.code()), (405, "METHOD_NOT_ALLOWED"));
}

#[tokio::test]
async fn invalid_products_and_quantities_never_enter_the_cart() {
    let app = spawn_app().await;
    let product = app.create_product("Widget", 1_000, 5).await;
    let retired = app.create_product("Retired", 1_000, 5).await;
    sqlx::query("UPDATE products SET active = false WHERE id = $1")
        .bind(retired)
        .execute(&app.pool)
        .await
        .unwrap();
    let cart = app.create_cart().await;
    let path = format!("/carts/{cart}/items");

    let cases = [
        (
            json!({ "product_id": Uuid::new_v4(), "quantity": 1 }),
            422,
            "UNKNOWN_PRODUCT",
        ),
        (
            json!({ "product_id": product, "quantity": 0 }),
            422,
            "VALIDATION_FAILED",
        ),
        (
            json!({ "product_id": product, "quantity": -3 }),
            422,
            "VALIDATION_FAILED",
        ),
        (
            json!({ "product_id": product, "quantity": 1001 }),
            422,
            "VALIDATION_FAILED",
        ),
        (
            json!({ "product_id": product, "quantity": 6 }),
            409,
            "INSUFFICIENT_INVENTORY",
        ),
        (
            json!({ "product_id": retired, "quantity": 1 }),
            409,
            "PRODUCT_UNAVAILABLE",
        ),
        (
            json!({ "product_id": product, "quantity": 1.5 }),
            400,
            "INVALID_REQUEST",
        ),
        (
            json!({ "product_id": product, "quantity": "2" }),
            400,
            "INVALID_REQUEST",
        ),
        (
            json!({ "product_id": "not-a-uuid", "quantity": 1 }),
            400,
            "INVALID_REQUEST",
        ),
        (json!({ "product_id": product }), 400, "INVALID_REQUEST"),
        (
            json!({ "product_id": product, "quantity": 1, "price_paise": 1 }),
            400,
            "INVALID_REQUEST",
        ),
    ];
    for (body, status, code) in cases {
        let r = app.post(&path, body.clone()).await;
        assert_eq!(
            (r.status.as_u16(), r.code()),
            (status, code),
            "body: {body}"
        );
    }

    let shortfall = app.add_item(cart, product, 6).await;
    assert_eq!(
        shortfall.body["error"]["details"]["items"],
        json!([{ "product_id": product, "requested": 6, "available": 5 }])
    );

    let missing_cart = app.add_item(Uuid::new_v4(), product, 1).await;
    assert_eq!(missing_cart.code(), "CART_NOT_FOUND");

    assert_eq!(app.scalar("SELECT count(*) FROM cart_items").await, 0);
}

#[tokio::test]
async fn items_can_be_added_updated_and_removed_with_correct_totals() {
    let app = spawn_app().await;
    let a = app.create_product("A", 1_250, 10).await;
    let b = app.create_product("B", 999, 10).await;
    let cart = app.create_cart().await;
    let item = |p: Uuid| format!("/carts/{cart}/items/{p}");

    assert_eq!(app.add_item(cart, a, 2).await.status, 201);
    let r = app.add_item(cart, b, 3).await;
    assert_eq!(r.status, 201);
    assert_eq!(r.body["subtotal_paise"], 2 * 1_250 + 3 * 999);
    assert_eq!(r.body["total_quantity"], 5);

    // Adding a product that is already in the cart is rejected rather than
    // incrementing, so a retried "add" cannot double the quantity.
    let dup = app.add_item(cart, a, 1).await;
    assert_eq!(
        (dup.status.as_u16(), dup.code()),
        (409, "ITEM_ALREADY_IN_CART")
    );

    // PATCH sets an absolute quantity, so repeating it is harmless.
    for _ in 0..2 {
        let r = app.patch(&item(a), json!({ "quantity": 4 })).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.body["items"][0]["quantity"], 4);
        assert_eq!(r.body["items"][0]["line_total_paise"], 5_000);
        assert_eq!(r.body["subtotal_paise"], 5_000 + 3 * 999);
    }
    let zero = app.patch(&item(a), json!({ "quantity": 0 })).await;
    assert_eq!(
        (zero.status.as_u16(), zero.code()),
        (422, "VALIDATION_FAILED")
    );
    let too_many = app.patch(&item(a), json!({ "quantity": 11 })).await;
    assert_eq!(
        (too_many.status.as_u16(), too_many.code()),
        (409, "INSUFFICIENT_INVENTORY")
    );

    let r = app.delete(&item(b)).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body["items"].as_array().unwrap().len(), 1);
    assert_eq!(r.body["subtotal_paise"], 5_000);

    let again = app.delete(&item(b)).await;
    assert_eq!(
        (again.status.as_u16(), again.code()),
        (404, "ITEM_NOT_IN_CART")
    );
    let patch_missing = app.patch(&item(b), json!({ "quantity": 1 })).await;
    assert_eq!(patch_missing.code(), "ITEM_NOT_IN_CART");

    // Failed mutations above changed nothing.
    let quantity: i64 = app
        .scalar(&format!(
            "SELECT quantity::bigint FROM cart_items WHERE cart_id = '{cart}'"
        ))
        .await;
    assert_eq!(quantity, 4);
}

#[tokio::test]
async fn cart_shows_current_prices_and_flags_lines_that_cannot_be_bought() {
    let app = spawn_app().await;
    let p = app.create_product("Lamp", 1_000, 5).await;
    let cart = app.cart_with(&[(p, 3)]).await;

    let r = app
        .patch(
            &format!("/admin/products/{p}"),
            json!({ "price_paise": 1_500, "stock_delta": -4 }),
        )
        .await;
    assert_eq!(r.status, 200);

    let view = app.get(&format!("/carts/{cart}")).await.body;
    assert_eq!(view["items"][0]["unit_price_paise"], 1_500);
    assert_eq!(view["subtotal_paise"], 4_500);
    assert_eq!(view["items"][0]["stock_available"], 1);
    assert_eq!(view["items"][0]["purchasable"], false);
}

#[tokio::test]
async fn admin_stock_adjustments_are_bounded_and_never_go_negative() {
    let app = spawn_app().await;
    let p = app.create_product("Bulb", 500, 5).await;
    let path = format!("/admin/products/{p}");

    let huge = app.patch(&path, json!({ "stock_delta": i32::MAX })).await;
    assert_eq!(
        (huge.status.as_u16(), huge.code()),
        (422, "VALIDATION_FAILED")
    );
    let negative = app.patch(&path, json!({ "stock_delta": -6 })).await;
    assert_eq!(
        (negative.status.as_u16(), negative.code()),
        (409, "INSUFFICIENT_INVENTORY")
    );
    let empty = app.patch(&path, json!({})).await;
    assert_eq!(
        (empty.status.as_u16(), empty.code()),
        (422, "VALIDATION_FAILED")
    );
    assert_eq!(app.stock(p).await, 5);

    let restock = app.patch(&path, json!({ "stock_delta": 10 })).await;
    assert_eq!(
        (restock.status.as_u16(), restock.body["stock"].as_i64()),
        (200, Some(15))
    );
}
