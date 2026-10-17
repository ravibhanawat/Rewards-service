use std::time::Duration;

use sqlx::{
    migrate::Migrator,
    postgres::{PgConnectOptions, PgPool, PgPoolOptions},
};

pub static MIGRATOR: Migrator = sqlx::migrate!();

pub async fn connect(options: PgConnectOptions, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    // Bound how long a request can wait on a row lock or run a statement, so
    // heavy contention surfaces as a retryable error instead of a hung request.
    let options = options.options([("lock_timeout", "5s"), ("statement_timeout", "15s")]);
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options)
        .await
}
