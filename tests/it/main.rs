//! Integration tests against a real PostgreSQL (see `docker-compose.yml`).
//! One test binary, so the shared harness is compiled once.

mod carts;
mod checkout;
mod common;
mod concurrency;
mod coupons;
mod reporting;
