use checkout_service::RewardPolicy;
use reqwest::Method;
use serde_json::json;
use uuid::Uuid;

use crate::common::{assert_immutable, spawn_app, spawn_app_with};

#[tokio::test]
async fn checkout_creates_an_order_whose_snapshot_survives_product_changes() {
    let app = spawn_app().await;
    let keyboard = app.create_product("Keyboard", 349_900, 10).await;
    let stickers = app.create_product("Stickers", 4_999, 10).await;
    let cart = app.cart_with(&[(keyboard, 2), (stickers, 3)]).await;

    let r = app.checkout(cart, "snapshot-1", json!({})).await;
    assert_eq!(r.status, 201, "{r:?}");
    let order = r.id();
    assert_eq!(
        r.header("location"),
        Some(format!("/orders/{order}").as_str())
    );
    assert_eq!(r.body["subtotal_paise"], 714_797);
    assert_eq!(r.body["discount_paise"], 0);
    assert_eq!(r.body["total_paise"], 714_797);
    assert_eq!(r.body["items"].as_array().unwrap().len(), 2);

    assert_eq!(app.stock(keyboard).await, 8);
    assert_eq!(app.stock(stickers).await, 7);
    let cart_view = app.get(&format!("/carts/{cart}")).await.body;
    assert_eq!(cart_view["status"], "checked_out");
    assert_eq!(cart_view["order_id"], order.to_string());

    // Renaming and repricing the product does not rewrite history...
    let edit = app
        .patch(
            &format!("/admin/products/{keyboard}"),
            json!({ "name": "Keyboard v2", "price_paise": 1 }),
        )
        .await;
    assert_eq!(edit.status, 200);
    let fetched = app.get(&format!("/orders/{order}")).await;
    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.body, r.body);

    // ...and the database refuses to, even if application code tried. The
    // message proves the immutability trigger fired, not some other CHECK/FK.
    for tamper in [
        "UPDATE orders SET created_at = now()",
        "UPDATE order_items SET unit_price_paise = 1",
        "DELETE FROM order_items",
        "DELETE FROM orders",
    ] {
        assert_immutable(&app, tamper).await;
    }

    let missing = app.get(&format!("/orders/{}", Uuid::new_v4())).await;
    assert_eq!(
        (missing.status.as_u16(), missing.code()),
        (404, "ORDER_NOT_FOUND")
    );
}

#[tokio::test]
async fn retrying_with_the_same_key_replays_the_order_without_side_effects() {
    let app = spawn_app().await;
    let p = app.create_product("Mug", 45_000, 5).await;
    let cart = app.cart_with(&[(p, 2)]).await;

    let first = app.checkout(cart, "retry-me", json!({})).await;
    assert_eq!(first.status, 201);

    for _ in 0..3 {
        let retry = app.checkout(cart, "retry-me", json!({})).await;
        assert_eq!(retry.status, 200);
        assert_eq!(retry.header("idempotent-replayed"), Some("true"));
        assert_eq!(retry.body, first.body);
    }
    assert_eq!(app.stock(p).await, 3);
    assert_eq!(app.scalar("SELECT count(*) FROM orders").await, 1);
}

#[tokio::test]
async fn reusing_a_key_for_a_different_request_is_rejected() {
    let app = spawn_app().await;
    let p = app.create_product("Pen", 1_000, 10).await;
    let cart = app.cart_with(&[(p, 1)]).await;
    assert_eq!(
        app.checkout(cart, "shared-key", json!({})).await.status,
        201
    );

    // Same key, same cart, different payload.
    for body in [
        json!({ "coupon_code": "ANY" }),
        json!({ "expected_subtotal_paise": 1_000 }),
    ] {
        let r = app.checkout(cart, "shared-key", body).await;
        assert_eq!(
            (r.status.as_u16(), r.code()),
            (422, "IDEMPOTENCY_KEY_REUSED")
        );
    }

    // Same key, different cart: must not return (or create) an order for it.
    let other = app.cart_with(&[(p, 1)]).await;
    let r = app.checkout(other, "shared-key", json!({})).await;
    assert_eq!(
        (r.status.as_u16(), r.code()),
        (422, "IDEMPOTENCY_KEY_REUSED")
    );
    assert_eq!(
        app.get(&format!("/carts/{other}")).await.body["status"],
        "open"
    );
    assert_eq!(app.scalar("SELECT count(*) FROM orders").await, 1);
    assert_eq!(app.stock(p).await, 9);
}

#[tokio::test]
async fn a_cart_is_checked_out_at_most_once_even_with_a_new_key() {
    let app = spawn_app().await;
    let p = app.create_product("Book", 59_900, 10).await;
    let cart = app.cart_with(&[(p, 1)]).await;

    assert_eq!(app.checkout(cart, "first", json!({})).await.status, 201);
    let second = app.checkout(cart, "second", json!({})).await;
    assert_eq!(
        (second.status.as_u16(), second.code()),
        (409, "CART_ALREADY_CHECKED_OUT")
    );

    assert_eq!(app.scalar("SELECT count(*) FROM orders").await, 1);
    assert_eq!(app.stock(p).await, 9);
}

#[tokio::test]
async fn invalid_checkout_requests_fail_without_side_effects() {
    let app = spawn_app().await;
    let p = app.create_product("Cap", 20_000, 10).await;
    let empty = app.create_cart().await;
    let cart = app.cart_with(&[(p, 2)]).await;
    let path = format!("/carts/{cart}/checkout");

    let r = app.checkout(empty, "k", json!({})).await;
    assert_eq!((r.status.as_u16(), r.code()), (409, "EMPTY_CART"));
    let r = app.checkout(Uuid::new_v4(), "k", json!({})).await;
    assert_eq!((r.status.as_u16(), r.code()), (404, "CART_NOT_FOUND"));
    let r = app.request(Method::POST, &path, None, &[]).await;
    assert_eq!(
        (r.status.as_u16(), r.code()),
        (400, "IDEMPOTENCY_KEY_MISSING")
    );
    let long_key = "x".repeat(256);
    let r = app.checkout(cart, &long_key, json!({})).await;
    assert_eq!(
        (r.status.as_u16(), r.code()),
        (400, "IDEMPOTENCY_KEY_INVALID")
    );
    let r = app
        .checkout(cart, "k", json!({ "coupon": "typo-in-field-name" }))
        .await;
    assert_eq!((r.status.as_u16(), r.code()), (400, "INVALID_REQUEST"));
    let r = app
        .checkout(cart, "k", json!({ "coupon_code": "  " }))
        .await;
    assert_eq!((r.status.as_u16(), r.code()), (422, "VALIDATION_FAILED"));
    let r = app
        .checkout(cart, "k", json!({ "coupon_code": "NOPE" }))
        .await;
    assert_eq!((r.status.as_u16(), r.code()), (422, "UNKNOWN_COUPON"));

    assert_eq!(app.scalar("SELECT count(*) FROM orders").await, 0);
    assert_eq!(app.stock(p).await, 10);

    // Failed attempts do not bind the key: the same key now succeeds.
    let ok = app.checkout(cart, "k", json!({})).await;
    assert_eq!(ok.status, 201, "{ok:?}");
}

#[tokio::test]
async fn failed_checkout_leaves_coupon_inventory_and_cart_untouched() {
    let app = spawn_app_with(RewardPolicy::new(1, 10).unwrap()).await;
    let plenty = app.create_product("Plenty", 10_000, 10).await;
    let scarce = app.create_product("Scarce", 5_000, 1).await;
    app.place_order(&[(plenty, 1)], None).await;
    let code = app.generate_coupon().await.body["coupon"]["code"]
        .as_str()
        .unwrap()
        .to_string();

    let cart = app.cart_with(&[(plenty, 2), (scarce, 1)]).await;
    // Someone else's purchase empties the scarce product after it was added.
    app.place_order(&[(scarce, 1)], None).await;

    let r = app
        .checkout(cart, "attempt", json!({ "coupon_code": code }))
        .await;
    assert_eq!(
        (r.status.as_u16(), r.code()),
        (409, "INSUFFICIENT_INVENTORY")
    );
    assert_eq!(
        r.body["error"]["details"]["items"],
        json!([{ "product_id": scarce, "requested": 1, "available": 0 }])
    );

    // No partial effects: the in-stock line was not decremented, the coupon
    // is still available and the cart is still open.
    assert_eq!(app.stock(plenty).await, 9);
    assert_eq!(
        app.get("/admin/coupons").await.body[0]["status"],
        "available"
    );
    assert_eq!(
        app.get(&format!("/carts/{cart}")).await.body["status"],
        "open"
    );

    // Fix the cart; the same key and coupon now succeed.
    assert_eq!(
        app.delete(&format!("/carts/{cart}/items/{scarce}"))
            .await
            .status,
        200
    );
    let ok = app
        .checkout(cart, "attempt", json!({ "coupon_code": code }))
        .await;
    assert_eq!(ok.status, 201, "{ok:?}");
    assert_eq!(ok.body["discount_paise"], 2_000);
    assert_eq!(ok.body["total_paise"], 18_000);
    assert_eq!(
        app.get("/admin/coupons").await.body[0]["status"],
        "redeemed"
    );
}

#[tokio::test]
async fn checkout_charges_the_current_price_unless_the_price_guard_disagrees() {
    let app = spawn_app().await;
    let p = app.create_product("Chair", 1_000, 10).await;
    let cart = app.cart_with(&[(p, 2)]).await;
    assert_eq!(
        app.get(&format!("/carts/{cart}")).await.body["subtotal_paise"],
        2_000
    );

    assert_eq!(
        app.patch(
            &format!("/admin/products/{p}"),
            json!({ "price_paise": 1_500 })
        )
        .await
        .status,
        200
    );

    let stale = app
        .checkout(cart, "guarded", json!({ "expected_subtotal_paise": 2_000 }))
        .await;
    assert_eq!(
        (stale.status.as_u16(), stale.code()),
        (409, "PRICE_CHANGED")
    );
    assert_eq!(
        stale.body["error"]["details"]["actual_subtotal_paise"],
        3_000
    );
    assert_eq!(app.stock(p).await, 10);

    let ok = app
        .checkout(
            cart,
            "guarded-2",
            json!({ "expected_subtotal_paise": 3_000 }),
        )
        .await;
    assert_eq!(ok.status, 201);
    assert_eq!(ok.body["items"][0]["unit_price_paise"], 1_500);
    assert_eq!(ok.body["total_paise"], 3_000);
}

#[tokio::test]
async fn discontinued_product_blocks_checkout() {
    let app = spawn_app().await;
    let p = app.create_product("Old model", 1_000, 10).await;
    let cart = app.cart_with(&[(p, 1)]).await;
    assert_eq!(
        app.patch(&format!("/admin/products/{p}"), json!({ "active": false }))
            .await
            .status,
        200
    );

    let r = app.checkout(cart, "k", json!({})).await;
    assert_eq!((r.status.as_u16(), r.code()), (409, "PRODUCT_UNAVAILABLE"));
    assert_eq!(r.body["error"]["details"]["product_ids"], json!([p]));
    assert_eq!(app.stock(p).await, 10);
}
