pub mod config;
pub mod db;
pub mod error;
mod extract;
pub mod models;
pub mod money;
mod routes;
pub mod services;

use sqlx::PgPool;

pub use config::RewardPolicy;
pub use routes::router;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub rewards: RewardPolicy,
}
