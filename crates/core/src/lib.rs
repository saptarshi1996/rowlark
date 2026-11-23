//! Rowlark core library.
//!
//! This crate must stay free of UI dependencies so it can be tested on its own
//! (and so the UI can be swapped without touching the drivers).
//!
//! - `value`: the `Value` enum, one cell of data.
//! - `driver`: the `Driver` trait and the types it uses.
//! - `postgres`: the PostgreSQL driver.

pub mod driver;
pub mod postgres;
pub mod value;

pub use driver::{
    Column, ConnectionConfig, Driver, DriverError, Flavor, QueryEvent, QuerySummary, ServerInfo,
};
pub use postgres::PostgresDriver;
pub use value::Value;

/// Returns the version of the core crate, taken from Cargo.toml at compile time.
/// A placeholder that proves the workspace wiring works end to end.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_not_empty() {
        assert!(!version().is_empty());
    }
}
