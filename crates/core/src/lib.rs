//! Rowlark core library.
//!
//! This crate must stay free of UI dependencies so it can be tested on its own
//! (and so the UI can be swapped without touching the drivers).
//!
//! - `value`: the `Value` enum, one cell of data.
//! - `connection`: how to reach a database (host, TLS, URLs).
//! - `driver`: the `Driver` trait and the types it uses.
//! - `schema`: the normalized model of a database's structure (tables, columns...).
//! - `postgres`: the PostgreSQL driver.

pub mod connection;
pub mod driver;
pub mod postgres;
pub mod schema;
pub mod value;

pub use connection::{ConnectionConfig, TlsConfig, TlsMode};
pub use driver::{
    Column, ColumnOrigin, Driver, DriverError, Flavor, Notice, NoticeSeverity, QueryEvent,
    QuerySummary, ServerInfo, TransactionState,
};
pub use postgres::PostgresDriver;
pub use schema::{ColumnInfo, ForeignKeyInfo, IndexInfo, TableDetails, TableInfo, TableKind};
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
