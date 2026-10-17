//! Competing and repeated operations, released together through a barrier on
//! a multi-threaded runtime, each on its own HTTP request and DB connection.
//! Every test asserts the final database state, not just status codes.

use std::collections::{BTreeSet, HashSet};

use checkout_service::RewardPolicy;
use serde_json::json;
use uuid::Uuid;

use crate::common::{race, spawn_app, spawn_app_with};

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_checkouts_never_oversell_limited_stock() {
    let app = spawn_app().await;
    let initial = 5;
    let scarce = app.create_product("Scarce", 99_900, initial).await;
    let mut carts = Vec::new();
    for _ in 0..20 {
        carts.push(app.cart_with(&[(scarce, 1)]).await);
    }

    let results = race(carts, |cart| {
        let app = app.clone();
        async move { app.checkout(cart, &format!("race-{cart}"), json!({})).await }
    })
    .await;

    let created = results.iter().filter(|r| r.status == 201).count();
    let sold_out = results
        .iter()
        .filter(|r| r.code() == "INSUFFICIENT_INVENTORY")
        .count();
    assert_eq!((created, sold_out), (5, 15), "{results:?}");

    let sold = app.scalar(&format!("SELECT COALESCE(sum(quantity), 0)::bigint FROM order_items WHERE product_id = '{scarce}'")).await;
    assert_eq!(app.stock(scarce).await, 0);
    assert_eq!(sold, i64::from(initial));
    assert_eq!(app.scalar("SELECT count(*) FROM orders").await, 5);
    assert_eq!(
        app.scalar("SELECT count(*) FROM carts WHERE status = 'checked_out'")
            .await,
        5
    );
}

/// Carts contain the same two products, added in opposite orders. If product
/// rows were locked in cart order rather than id order, these would deadlock
/// and surface as CONCURRENT_UPDATE.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn overlapping_carts_do_not_deadlock_or_oversell() {
    let app = spawn_app().await;
    let a = app.create_product("A", 1_000, 8).await;
    let b = app.create_product("B", 2_000, 12).await;
    let mut carts = Vec::new();
    for i in 0..20 {
        let items = if i % 2 == 0 {
            [(a, 1), (b, 1)]
        } else {
            [(b, 1), (a, 1)]
        };
        carts.push(app.cart_with(&items).await);
    }

    let results = race(carts, |cart| {
        let app = app.clone();
        async move { app.checkout(cart, &format!("k-{cart}"), json!({})).await }
    })
    .await;

    let created = results.iter().filter(|r| r.status == 201).count();
    let sold_out = results
        .iter()
        .filter(|r| r.code() == "INSUFFICIENT_INVENTORY")
        .count();
    assert_eq!((created, sold_out), (8, 12), "{results:?}");
    assert_eq!(app.stock(a).await, 0);
    assert_eq!(app.stock(b).await, 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_retries_of_one_checkout_create_exactly_one_order() {
    let app = spawn_app().await;
    let p = app.create_product("Lamp", 10_000, 10).await;
    let cart = app.cart_with(&[(p, 3)]).await;

    let results = race((0..10).collect(), |_| {
        let app = app.clone();
        async move { app.checkout(cart, "same-key", json!({})).await }
    })
    .await;

    let created = results.iter().filter(|r| r.status == 201).count();
    let replayed = results
        .iter()
        .filter(|r| r.status == 200 && r.header("idempotent-replayed") == Some("true"))
        .count();
    assert_eq!((created, replayed), (1, 9), "{results:?}");
    let order_ids: HashSet<Uuid> = results.iter().map(|r| r.id()).collect();
    assert_eq!(
        order_ids.len(),
        1,
        "every response must describe the same order"
    );

    assert_eq!(app.scalar("SELECT count(*) FROM orders").await, 1);
    assert_eq!(app.stock(p).await, 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_checkouts_of_one_cart_with_different_keys_create_one_order() {
    let app = spawn_app().await;
    let p = app.create_product("Lamp", 10_000, 10).await;
    let cart = app.cart_with(&[(p, 3)]).await;

    let results = race((0..10).collect(), |i: i32| {
        let app = app.clone();
        async move { app.checkout(cart, &format!("key-{i}"), json!({})).await }
    })
    .await;

    let winners: Vec<usize> = (0..results.len())
        .filter(|&i| results[i].status == 201)
        .collect();
    let rejected = results
        .iter()
        .filter(|r| r.code() == "CART_ALREADY_CHECKED_OUT")
        .count();
    assert_eq!((winners.len(), rejected), (1, 9), "{results:?}");

    let stored_key: String = sqlx::query_scalar("SELECT idempotency_key FROM orders")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(stored_key, format!("key-{}", winners[0]));
    assert_eq!(app.stock(p).await, 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_coupon_is_redeemed_by_at_most_one_of_many_concurrent_checkouts() {
    let app = spawn_app_with(RewardPolicy::new(1, 10).unwrap()).await;
    let p = app.create_product("Plenty", 10_000, 100).await;
    app.place_order(&[(p, 1)], None).await;
    let code = app.generate_coupon().await.body["coupon"]["code"]
        .as_str()
        .unwrap()
        .to_string();
    let mut carts = Vec::new();
    for _ in 0..10 {
        carts.push(app.cart_with(&[(p, 1)]).await);
    }

    let results = race(carts, |cart| {
        let (app, code) = (app.clone(), code.clone());
        async move {
            app.checkout(cart, &format!("c-{cart}"), json!({ "coupon_code": code }))
                .await
        }
    })
    .await;

    let created = results.iter().filter(|r| r.status == 201).count();
    let rejected = results
        .iter()
        .filter(|r| r.code() == "COUPON_ALREADY_REDEEMED")
        .count();
    assert_eq!((created, rejected), (1, 9), "{results:?}");

    assert_eq!(
        app.scalar("SELECT count(*) FROM orders WHERE coupon_id IS NOT NULL")
            .await,
        1
    );
    assert_eq!(
        app.scalar("SELECT count(*) FROM carts WHERE status = 'open'")
            .await,
        9
    );
    assert_eq!(
        app.stock(p).await,
        100 - 1 - 1,
        "only the first order and the winner took stock"
    );
}

/// Checkouts that lock the coupon and then fail on inventory must release it:
/// the one valid checkout succeeds no matter where it lands in the queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn checkouts_that_fail_while_holding_the_coupon_do_not_consume_it() {
    let app = spawn_app_with(RewardPolicy::new(1, 10).unwrap()).await;
    let plenty = app.create_product("Plenty", 10_000, 100).await;
    let gone = app.create_product("Gone", 10_000, 9).await;
    app.place_order(&[(plenty, 1)], None).await;
    let code = app.generate_coupon().await.body["coupon"]["code"]
        .as_str()
        .unwrap()
        .to_string();

    let mut carts = Vec::new();
    for _ in 0..9 {
        carts.push(app.cart_with(&[(plenty, 1), (gone, 1)]).await);
    }
    let valid = app.cart_with(&[(plenty, 1)]).await;
    carts.push(valid);
    sqlx::query("UPDATE products SET stock = 0 WHERE id = $1")
        .bind(gone)
        .execute(&app.pool)
        .await
        .unwrap();

    let results = race(carts.clone(), |cart| {
        let (app, code) = (app.clone(), code.clone());
        async move {
            app.checkout(cart, &format!("c-{cart}"), json!({ "coupon_code": code }))
                .await
        }
    })
    .await;

    assert_eq!(
        results[9].status, 201,
        "the valid checkout must win the coupon: {:?}",
        results[9]
    );
    for r in &results[..9] {
        assert!(
            matches!(
                r.code(),
                "INSUFFICIENT_INVENTORY" | "COUPON_ALREADY_REDEEMED"
            ),
            "{r:?}"
        );
    }
    assert_eq!(app.stock(plenty).await, 100 - 1 - 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_coupon_generation_never_duplicates_a_milestone() {
    let app = spawn_app_with(RewardPolicy::new(2, 10).unwrap()).await;
    let p = app.create_product("Tea", 1_000, 100).await;
    for _ in 0..4 {
        app.place_order(&[(p, 1)], None).await;
    }

    let results = race((0..10).collect(), |_: i32| {
        let app = app.clone();
        async move { app.generate_coupon().await }
    })
    .await;

    let milestones: BTreeSet<i64> = results
        .iter()
        .filter(|r| r.status == 201)
        .map(|r| r.body["coupon"]["milestone_order_count"].as_i64().unwrap())
        .collect();
    let none_left = results
        .iter()
        .filter(|r| r.code() == "NO_ELIGIBLE_MILESTONE")
        .count();
    assert_eq!(milestones, BTreeSet::from([2, 4]));
    assert_eq!(none_left, 8, "{results:?}");
    assert_eq!(app.scalar("SELECT count(*) FROM coupons").await, 2);
}

/// Adding an item races with checkout. Whatever the interleaving, the order
/// must contain exactly what the cart contains: nothing may slip into a cart
/// after its contents were read for the order.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn items_added_during_checkout_are_either_ordered_or_rejected() {
    let app = spawn_app().await;
    let a = app.create_product("A", 1_000, 1_000).await;
    let b = app.create_product("B", 1_000, 1_000).await;

    for round in 0..15 {
        let cart = app.cart_with(&[(a, 1)]).await;
        let results = race(vec![true, false], |is_checkout| {
            let app = app.clone();
            async move {
                if is_checkout {
                    app.checkout(cart, &format!("r{round}"), json!({})).await
                } else {
                    app.add_item(cart, b, 1).await
                }
            }
        })
        .await;
        let (checkout, add) = (&results[0], &results[1]);
        assert_eq!(checkout.status, 201, "{checkout:?}");
        assert!(
            add.status == 201 || add.code() == "CART_ALREADY_CHECKED_OUT",
            "{add:?}"
        );

        let ordered = checkout.body["items"].as_array().unwrap().len();
        let in_cart = app
            .scalar(&format!(
                "SELECT count(*) FROM cart_items WHERE cart_id = '{cart}'"
            ))
            .await;
        assert_eq!(
            ordered as i64, in_cart,
            "round {round}: order and cart disagree"
        );
        assert_eq!(ordered, if add.status == 201 { 2 } else { 1 });
    }
}

/// The same Idempotency-Key sent for two *different* carts at the same
/// instant: the cart locks don't serialize them, so the unique index on
/// orders.idempotency_key decides. Exactly one order, the other gets 422.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn one_key_racing_on_two_carts_creates_exactly_one_order() {
    let app = spawn_app().await;
    let p = app.create_product("Pen", 1_000, 1_000).await;

    for round in 0..10 {
        let carts = vec![
            app.cart_with(&[(p, 1)]).await,
            app.cart_with(&[(p, 1)]).await,
        ];
        let results = race(carts.clone(), |cart| {
            let app = app.clone();
            async move {
                app.checkout(cart, &format!("shared-{round}"), json!({}))
                    .await
            }
        })
        .await;

        let mut outcome: Vec<(u16, String)> = results
            .iter()
            .map(|r| (r.status.as_u16(), r.code().to_string()))
            .collect();
        outcome.sort();
        assert_eq!(
            outcome,
            [
                (201, String::new()),
                (422, "IDEMPOTENCY_KEY_REUSED".to_string())
            ],
            "round {round}"
        );
        let open = app
            .scalar(&format!(
                "SELECT count(*) FROM carts WHERE status = 'open' AND id IN ('{}', '{}')",
                carts[0], carts[1]
            ))
            .await;
        assert_eq!(open, 1, "round {round}: the losing cart must stay open");
    }
    assert_eq!(app.scalar("SELECT count(*) FROM orders").await, 10);
    assert_eq!(app.stock(p).await, 1_000 - 10);
}

/// Reports read while checkouts are committing must still be internally
/// consistent: every section comes from the same snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn reports_taken_during_concurrent_checkouts_are_internally_consistent() {
    let app = spawn_app().await;
    let a = app.create_product("A", 1_999, 1_000).await;
    let b = app.create_product("B", 3_500, 1_000).await;
    let mut carts = Vec::new();
    for i in 0..30 {
        carts.push(app.cart_with(&[(a, 1 + i % 3), (b, 1)]).await);
    }

    let jobs: Vec<Option<Uuid>> = carts
        .into_iter()
        .map(Some)
        .chain((0..30).map(|_| None))
        .collect();
    let results = race(jobs, |job| {
        let app = app.clone();
        async move {
            match job {
                Some(cart) => app.checkout(cart, &format!("k-{cart}"), json!({})).await,
                None => app.get("/admin/reports/summary").await,
            }
        }
    })
    .await;

    for r in results.iter().filter(|r| r.body.get("products").is_some()) {
        let gross = r.body["gross_revenue_paise"].as_i64().unwrap();
        let by_product: i64 = r.body["products"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["gross_revenue_paise"].as_i64().unwrap())
            .sum();
        let units: i64 = r.body["products"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["quantity_sold"].as_i64().unwrap())
            .sum();
        let orders = r.body["successful_orders"].as_i64().unwrap();
        assert_eq!(
            by_product, gross,
            "per-product revenue must match the order totals in the same report"
        );
        // Every order here buys exactly one B, so B's units equal the order count.
        let b_units = r.body["products"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["product_id"] == b.to_string())
            .unwrap()["quantity_sold"]
            .as_i64()
            .unwrap();
        assert_eq!(b_units, orders);
        assert!(units >= 2 * orders);
    }
    assert_eq!(results.iter().filter(|r| r.status == 201).count(), 30);
}
