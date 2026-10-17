use checkout_service::RewardPolicy;
use serde_json::json;

use crate::common::spawn_app_with;

#[tokio::test]
async fn coupon_becomes_available_exactly_at_the_milestone() {
    let app = spawn_app_with(RewardPolicy::new(3, 10).unwrap()).await;
    let p = app.create_product("Tea", 25_000, 100).await;

    for _ in 0..2 {
        app.place_order(&[(p, 1)], None).await;
    }
    let early = app.generate_coupon().await;
    assert_eq!(
        (early.status.as_u16(), early.code()),
        (409, "NO_ELIGIBLE_MILESTONE")
    );
    assert_eq!(
        early.body["error"]["details"],
        json!({ "successful_orders": 2, "every_n_orders": 3, "next_milestone_order_count": 3, "orders_until_next_milestone": 1 })
    );

    app.place_order(&[(p, 1)], None).await; // the 3rd successful order
    let r = app.generate_coupon().await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(r.body["coupon"]["milestone_order_count"], 3);
    assert_eq!(r.body["coupon"]["discount_percent"], 10);
    assert_eq!(r.body["coupon"]["status"], "available");
    assert_eq!(r.body["successful_orders"], 3);
    assert_eq!(r.body["remaining_eligible_milestones"], 0);

    // The milestone is rewarded once; the next one is 6.
    let again = app.generate_coupon().await;
    assert_eq!(again.code(), "NO_ELIGIBLE_MILESTONE");
    assert_eq!(
        again.body["error"]["details"]["next_milestone_order_count"],
        6
    );
    assert_eq!(app.scalar("SELECT count(*) FROM coupons").await, 1);
}

#[tokio::test]
async fn failed_checkouts_do_not_count_towards_milestones() {
    let app = spawn_app_with(RewardPolicy::new(2, 10).unwrap()).await;
    let p = app.create_product("Rare", 1_000, 1).await;
    app.place_order(&[(p, 1)], None).await;

    // A second checkout attempt fails (out of stock): still only 1 order.
    let p2 = app.create_product("Rarer", 1_000, 1).await;
    let cart = app.cart_with(&[(p2, 1)]).await;
    app.place_order(&[(p2, 1)], None).await;
    assert_eq!(
        app.checkout(cart, "fails", json!({})).await.code(),
        "INSUFFICIENT_INVENTORY"
    );
    let r = app.generate_coupon().await;
    assert_eq!(
        r.status, 201,
        "2 successful orders reach milestone 2: {r:?}"
    );
    assert_eq!(app.scalar("SELECT count(*) FROM orders").await, 2);
}

#[tokio::test]
async fn delayed_generation_rewards_each_missed_milestone_once_in_order() {
    let app = spawn_app_with(RewardPolicy::new(2, 10).unwrap()).await;
    let p = app.create_product("Tea", 25_000, 100).await;
    for _ in 0..5 {
        app.place_order(&[(p, 1)], None).await;
    }

    let first = app.generate_coupon().await;
    assert_eq!(first.body["coupon"]["milestone_order_count"], 2);
    assert_eq!(first.body["remaining_eligible_milestones"], 1);
    let second = app.generate_coupon().await;
    assert_eq!(second.body["coupon"]["milestone_order_count"], 4);
    assert_eq!(second.body["remaining_eligible_milestones"], 0);
    let third = app.generate_coupon().await;
    assert_eq!(third.code(), "NO_ELIGIBLE_MILESTONE");
    assert_eq!(
        third.body["error"]["details"]["next_milestone_order_count"],
        6
    );
}

#[tokio::test]
async fn discount_rounds_half_up_and_a_coupon_is_single_use() {
    let app = spawn_app_with(RewardPolicy::new(1, 15).unwrap()).await;
    let filler = app.create_product("Filler", 100, 100).await;
    let p = app.create_product("Odd price", 1_010, 100).await;
    app.place_order(&[(filler, 1)], None).await;
    let code = app.generate_coupon().await.body["coupon"]["code"]
        .as_str()
        .unwrap()
        .to_string();

    // Codes are case-insensitive and trimmed. 15% of 1010 = 151.5 -> 152.
    let order = app
        .place_order(&[(p, 1)], Some(&format!("  {}  ", code.to_lowercase())))
        .await;
    assert_eq!(order["subtotal_paise"], 1_010);
    assert_eq!(order["discount_paise"], 152);
    assert_eq!(order["total_paise"], 858);
    assert_eq!(
        order["coupon"],
        json!({ "code": code, "discount_percent": 15 })
    );

    let cart = app.cart_with(&[(p, 1)]).await;
    let reuse = app
        .checkout(cart, "reuse", json!({ "coupon_code": code }))
        .await;
    assert_eq!(
        (reuse.status.as_u16(), reuse.code()),
        (409, "COUPON_ALREADY_REDEEMED")
    );
    assert_eq!(app.stock(p).await, 99);

    let coupons = app.get("/admin/coupons").await.body;
    assert_eq!(coupons[0]["status"], "redeemed");
    assert_eq!(coupons[0]["redeemed_order_id"], order["id"]);
}

#[tokio::test]
async fn a_full_discount_yields_a_zero_total_never_a_negative_one() {
    let app = spawn_app_with(RewardPolicy::new(1, 100).unwrap()).await;
    let p = app.create_product("Gift", 99_999, 10).await;
    app.place_order(&[(p, 1)], None).await;
    let code = app.generate_coupon().await.body["coupon"]["code"]
        .as_str()
        .unwrap()
        .to_string();

    let order = app.place_order(&[(p, 3)], Some(&code)).await;
    assert_eq!(order["subtotal_paise"], 299_997);
    assert_eq!(order["discount_paise"], 299_997);
    assert_eq!(order["total_paise"], 0);
}
