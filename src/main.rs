use std::str::FromStr;

use checkout_service::{AppState, config::Config, db, router};
use sqlx::postgres::PgConnectOptions;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn")))
        .init();

    let config = Config::from_env()?;
    let pool = db::connect(PgConnectOptions::from_str(&config.database_url)?, config.db_max_connections).await?;
    db::MIGRATOR.run(&pool).await?;
    tracing::info!(rewards = ?config.rewards, "migrations applied");

    let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, router(AppState { pool, rewards: config.rewards }))
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await?;
    Ok(())
}
