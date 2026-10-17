use std::{env, net::SocketAddr};

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: SocketAddr,
    pub db_max_connections: u32,
    pub rewards: RewardPolicy,
}

/// The `n` and `x` of the business rules: every `every_n_orders`-th successful
/// order makes one coupon worth `discount_percent` % available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RewardPolicy {
    pub every_n_orders: i64,
    pub discount_percent: i32,
}

impl RewardPolicy {
    pub fn new(every_n_orders: i64, discount_percent: i32) -> Result<Self, String> {
        if every_n_orders < 1 {
            return Err(format!("every_n_orders must be >= 1, got {every_n_orders}"));
        }
        if !(1..=100).contains(&discount_percent) {
            return Err(format!("discount_percent must be within 1..=100, got {discount_percent}"));
        }
        Ok(Self { every_n_orders, discount_percent })
    }
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let database_url = env::var("DATABASE_URL").map_err(|_| "DATABASE_URL must be set".to_string())?;
        let bind_addr = parse_var("BIND_ADDR", "127.0.0.1:8080")?;
        let db_max_connections = parse_var("DB_MAX_CONNECTIONS", "20")?;
        let rewards = RewardPolicy::new(
            parse_var("REWARD_EVERY_N_ORDERS", "5")?,
            parse_var("REWARD_DISCOUNT_PERCENT", "10")?,
        )?;
        Ok(Self { database_url, bind_addr, db_max_connections, rewards })
    }
}

fn parse_var<T: std::str::FromStr>(name: &str, default: &str) -> Result<T, String> {
    let raw = env::var(name).unwrap_or_else(|_| default.to_string());
    raw.parse().map_err(|_| format!("{name} has an invalid value: {raw:?}"))
}
