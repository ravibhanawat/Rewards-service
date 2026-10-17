use checkout_service::RewardPolicy;
use serde_json::{Value, json};

use crate::common::spawn_app_with;

#[tokio::test]
async fn report_reconciles_with_orders_and_coupons_and_never_mutates_state() {
    let app = spawn_app_with(RewardPolicy::new(2, 10).unwrap()).await;
    let a = app.create_product("A", 1_999, 100).await;
    let b = app.create_product("B", 35_000, 100).await;
    let unsold = app.create_product("C", 500, 100).await;
    let scarce = app.create_product("D", 700, 0).await;

    app.place_order(&[(a, 3)], None).await; //                     5_997
    app.place_order(&[(b, 1)], None).await; //                    35_000
    let coupon1 = app.generate_coupon().await.body["coupon"]["code"]
        .as_str()
        .unwrap()
        .to_string();
    // 1_999 + 2 * 35_000 = 71_999; 10% = 7_199.9 -> 7_200; net 64_799
    app.place_order(&[(a, 1), (b, 2)], Some(&coupon1)).await;
    app.place_order(&[(a, 2)], None).await; //                     3_998
    assert_eq!(app.generate_coupon().await.status, 201); //       coupon 2, unused

    // A failed checkout must not show up anywhere in the report.
    let failing = app.cart_with(&[(a, 1)]).await;
    sqlx::query("INSERT INTO cart_items (cart_id, product_id, quantity) VALUES ($1, $2, 1)")
        .bind(failing)
        .bind(scarce)
        .execute(&app.pool)
        .await
        .unwrap();
    assert_eq!(
        app.checkout(failing, "fails", json!({})).await.code(),
        "INSUFFICIENT_INVENTORY"
    );

    let report = app.get("/admin/reports/summary").await;
    assert_eq!(report.status, 200);
    let r = &report.body;
    assert_eq!(r["successful_orders"], 4);
    assert_eq!(r["gross_revenue_paise"], 5_997 + 35_000 + 71_999 + 3_998);
    assert_eq!(r["total_discounts_paise"], 7_200);
    assert_eq!(r["net_revenue_paise"], 116_994 - 7_200);
    assert_eq!(
        r["coupons"],
        json!({ "generated": 2, "available": 1, "redeemed": 1 })
    );
    let sales = |id: uuid::Uuid| {
        r["products"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["product_id"] == id.to_string())
            .cloned()
    };
    assert_eq!(sales(a).unwrap()["quantity_sold"], 6);
    assert_eq!(sales(a).unwrap()["gross_revenue_paise"], 6 * 1_999);
    assert_eq!(sales(b).unwrap()["quantity_sold"], 3);
    assert_eq!(sales(unsold).unwrap()["quantity_sold"], 0);
    assert_eq!(sales(scarce).unwrap()["quantity_sold"], 0);

    // Independently recompute everything from the orders and coupons the API
    // itself returns.
    let orders = app.get("/admin/orders").await.body;
    let orders = orders.as_array().unwrap();
    let sum = |field: &str| {
        orders
            .iter()
            .map(|o| o[field].as_i64().unwrap())
            .sum::<i64>()
    };
    assert_eq!(
        orders.len() as i64,
        r["successful_orders"].as_i64().unwrap()
    );
    assert_eq!(
        sum("subtotal_paise"),
        r["gross_revenue_paise"].as_i64().unwrap()
    );
    assert_eq!(
        sum("discount_paise"),
        r["total_discounts_paise"].as_i64().unwrap()
    );
    assert_eq!(sum("total_paise"), r["net_revenue_paise"].as_i64().unwrap());
    for order in orders {
        let lines: i64 = order["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["line_total_paise"].as_i64().unwrap())
            .sum();
        assert_eq!(
            lines,
            order["subtotal_paise"].as_i64().unwrap(),
            "order {} lines != subtotal",
            order["id"]
        );
    }
    let per_product: i64 = r["products"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["gross_revenue_paise"].as_i64().unwrap())
        .sum();
    assert_eq!(per_product, r["gross_revenue_paise"].as_i64().unwrap());
    let coupons = app.get("/admin/coupons").await.body;
    let redeemed = coupons
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["status"] == "redeemed")
        .count();
    assert_eq!(redeemed as i64, r["coupons"]["redeemed"].as_i64().unwrap());
    assert_eq!(
        coupons.as_array().unwrap().len() as i64,
        r["coupons"]["generated"].as_i64().unwrap()
    );

    // Repeated report requests are identical and change nothing.
    let fingerprint =
        "SELECT (SELECT count(*) FROM orders) * 1000000 + (SELECT count(*) FROM coupons) * 1000
                            + (SELECT sum(stock) FROM products)";
    let before = app.scalar(fingerprint).await;
    for _ in 0..5 {
        let again: Value = app.get("/admin/reports/summary").await.body;
        assert_eq!(&again, r);
    }
    assert_eq!(app.scalar(fingerprint).await, before);
}
