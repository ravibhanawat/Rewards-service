//! Test harness: every test gets its own freshly migrated PostgreSQL database
//! and its own server on a random port, so tests run in parallel without
//! sharing state and concurrent requests go through real HTTP and real
//! database connections.

use std::{str::FromStr, sync::Arc};

use checkout_service::{AppState, RewardPolicy, db, router};
use reqwest::{Client, Method, StatusCode, header::HeaderMap};
use serde_json::{Value, json};
use sqlx::{Connection, Executor, PgConnection, PgPool, postgres::PgConnectOptions};
use tokio::{
    sync::{Barrier, OnceCell},
    task::JoinHandle,
};
use uuid::Uuid;

const DEFAULT_DATABASE_URL: &str = "postgres://checkout:checkout@localhost:54329/checkout";
const TEST_DB_PREFIX: &str = "checkout_test_";

static STALE_DATABASES_DROPPED: OnceCell<()> = OnceCell::const_new();

#[derive(Clone)]
pub struct TestApp {
    pub base_url: String,
    pub client: Client,
    pub pool: PgPool,
}

#[derive(Debug)]
pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Value,
}

impl Resp {
    /// The machine-readable error code, or "" for a success response.
    pub fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }

    pub fn id(&self) -> Uuid {
        self.body["id"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("no id in {self:?}"))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

/// Default reward policy for tests: a coupon every 3 orders, 10% off.
pub async fn spawn_app() -> TestApp {
    spawn_app_with(RewardPolicy::new(3, 10).unwrap()).await
}

pub async fn spawn_app_with(rewards: RewardPolicy) -> TestApp {
    let _ = dotenvy::dotenv();
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string());
    let server = PgConnectOptions::from_str(&url).expect("valid DATABASE_URL");
    let maintenance = server.clone().database("postgres");

    STALE_DATABASES_DROPPED
        .get_or_init(|| drop_test_databases(&maintenance))
        .await;

    let db_name = format!("{TEST_DB_PREFIX}{}", Uuid::new_v4().simple());
    let mut admin = PgConnection::connect_with(&maintenance)
        .await
        .expect("cannot reach PostgreSQL; did you run `docker compose up -d`?");
    admin
        .execute(format!(r#"CREATE DATABASE "{db_name}""#).as_str())
        .await
        .unwrap();
    admin.close().await.ok();

    let pool = db::connect(server.database(&db_name), 20).await.unwrap();
    db::MIGRATOR.run(&pool).await.unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(AppState {
        pool: pool.clone(),
        rewards,
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    TestApp {
        base_url: format!("http://{addr}"),
        client: Client::new(),
        pool,
    }
}

/// Databases from earlier runs are dropped once per test binary. They are
/// deliberately kept after a run so a failing test's state can be inspected.
async fn drop_test_databases(maintenance: &PgConnectOptions) {
    let mut conn = PgConnection::connect_with(maintenance)
        .await
        .expect("cannot reach PostgreSQL; did you run `docker compose up -d`?");
    let stale: Vec<String> =
        sqlx::query_scalar("SELECT datname FROM pg_database WHERE starts_with(datname, $1)")
            .bind(TEST_DB_PREFIX)
            .fetch_all(&mut conn)
            .await
            .unwrap();
    for name in stale {
        conn.execute(format!(r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#).as_str())
            .await
            .unwrap();
    }
}

/// Runs every task at the same instant (as far as the scheduler allows) and
/// returns their results in input order.
pub async fn race<T, F, Fut>(inputs: Vec<T>, f: F) -> Vec<Fut::Output>
where
    T: Send + 'static,
    F: Fn(T) -> Fut,
    Fut: Future + Send + 'static,
    Fut::Output: Send + 'static,
{
    let barrier = Arc::new(Barrier::new(inputs.len()));
    let handles: Vec<JoinHandle<Fut::Output>> = inputs
        .into_iter()
        .map(|input| {
            let barrier = barrier.clone();
            let task = f(input);
            tokio::spawn(async move {
                barrier.wait().await;
                task.await
            })
        })
        .collect();
    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        results.push(handle.await.expect("task panicked"));
    }
    results
}

impl TestApp {
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Resp {
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.base_url));
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let res = req.send().await.expect("request failed");
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = res.bytes().await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Resp {
            status,
            headers,
            body,
        }
    }

    pub async fn get(&self, path: &str) -> Resp {
        self.request(Method::GET, path, None, &[]).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Resp {
        self.request(Method::POST, path, Some(body), &[]).await
    }

    pub async fn patch(&self, path: &str, body: Value) -> Resp {
        self.request(Method::PATCH, path, Some(body), &[]).await
    }

    pub async fn delete(&self, path: &str) -> Resp {
        self.request(Method::DELETE, path, None, &[]).await
    }

    /// Inserted directly so tests control exact prices and stock.
    pub async fn create_product(&self, name: &str, price_paise: i64, stock: i32) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO products (id, name, price_paise, stock) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(name)
            .bind(price_paise)
            .bind(stock)
            .execute(&self.pool)
            .await
            .unwrap();
        id
    }

    pub async fn create_cart(&self) -> Uuid {
        let r = self.post("/carts", json!({})).await;
        assert_eq!(r.status, 201, "{r:?}");
        r.id()
    }

    pub async fn add_item(&self, cart: Uuid, product: Uuid, quantity: i64) -> Resp {
        self.post(
            &format!("/carts/{cart}/items"),
            json!({ "product_id": product, "quantity": quantity }),
        )
        .await
    }

    /// A cart containing `items`, ready for checkout.
    pub async fn cart_with(&self, items: &[(Uuid, i64)]) -> Uuid {
        let cart = self.create_cart().await;
        for &(product, quantity) in items {
            let r = self.add_item(cart, product, quantity).await;
            assert_eq!(r.status, 201, "{r:?}");
        }
        cart
    }

    pub async fn checkout(&self, cart: Uuid, key: &str, body: Value) -> Resp {
        self.request(
            Method::POST,
            &format!("/carts/{cart}/checkout"),
            Some(body),
            &[("Idempotency-Key", key)],
        )
        .await
    }

    /// Creates a cart with `items` and checks it out; panics unless it succeeds.
    pub async fn place_order(&self, items: &[(Uuid, i64)], coupon: Option<&str>) -> Value {
        let cart = self.cart_with(items).await;
        let body = coupon.map_or_else(|| json!({}), |code| json!({ "coupon_code": code }));
        let r = self.checkout(cart, &format!("order-{cart}"), body).await;
        assert_eq!(r.status, 201, "{r:?}");
        r.body
    }

    pub async fn generate_coupon(&self) -> Resp {
        self.post("/admin/coupons/generate", json!({})).await
    }

    pub async fn stock(&self, product: Uuid) -> i64 {
        self.scalar(&format!(
            "SELECT stock::bigint FROM products WHERE id = '{product}'"
        ))
        .await
    }

    /// Runs a query returning a single BIGINT (tests only; SQL is trusted).
    pub async fn scalar(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
    }
}
