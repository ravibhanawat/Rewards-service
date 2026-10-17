//! Business operations. Each public function owns its transaction boundary;
//! SQL lives next to the rule it enforces rather than behind a repository
//! layer, because the locking clauses *are* the concurrency design.

pub mod carts;
pub mod checkout;
pub mod coupons;
pub mod orders;
pub mod products;
pub mod reports;
