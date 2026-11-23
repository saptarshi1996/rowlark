//! The `Driver` trait: the one place the rest of Rowlark talks to a database.
//!
//! The UI is written against this trait and the types below, never against
//! Postgres or MySQL directly. Each database gets its own driver that
//! implements the trait and hides its quirks (see `postgres.rs`).
//!
//! The trait starts small on purpose (ping and run a query). `cancel` and
//! `introspect` (tables, columns, indexes) are added in later steps.

use std::future::Future;

use thiserror::Error;
use tokio::sync::mpsc;

use crate::value::Value;

/// Which database family we are talking to. Detected on connect from the
/// server's version string, because MariaDB speaks the MySQL protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Postgres,
    MySql,
    MariaDb,
}

/// Where and how to connect.
///
/// The password is deliberately NOT a field here. It lives in the OS keychain
/// and is passed to `connect` separately, so it cannot end up in a log line
/// through `{:?}` printing or in a saved config file by accident.
#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: String,
}

/// What `ping` learns about the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub flavor: Flavor,
    /// The server's own version text, e.g. "PostgreSQL 16.4 on aarch64-...".
    pub version: String,
}

/// One result column: its name and the server's name for its type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub type_name: String,
}

/// What a running query sends to the caller through a channel.
///
/// A channel is a pipe: the driver puts messages in one end while the UI
/// reads them from the other, so rows appear while the query is still running.
/// For each result set the order is: one `Columns`, then zero or more `Rows`.
#[derive(Debug)]
pub enum QueryEvent {
    Columns(Vec<Column>),
    /// A batch of rows (about 1,000 at a time). Each row has one `Value` per column.
    Rows(Vec<Vec<Value>>),
}

/// What the driver reports once the query has finished.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuerySummary {
    /// Total rows sent through the channel.
    pub rows_returned: u64,
    /// Rows changed by INSERT/UPDATE/DELETE (0 for a plain SELECT).
    pub rows_affected: u64,
}

/// Everything that can go wrong. Messages from the server are kept verbatim,
/// because Rowlark shows the database's own answer rather than rewording it.
#[derive(Debug, Error)]
pub enum DriverError {
    /// Could not reach the server or log in (network, wrong password, ...).
    #[error("connection failed: {0}")]
    Connection(String),
    /// The server rejected or failed the statement. `code` is the SQLSTATE
    /// (e.g. "42501" for permission denied) when the server gave one.
    #[error("{message}")]
    Server {
        code: Option<String>,
        message: String,
    },
    /// The user (or `cancel`) stopped the query and the server confirmed it.
    /// This is not a failure, so the UI can show it differently from `Server`.
    #[error("query cancelled")]
    Cancelled,
    /// The UI stopped listening (closed the tab), so the query was abandoned.
    #[error("the result receiver was closed")]
    ReceiverClosed,
}

/// The operations every database driver provides.
///
/// Why `-> impl Future<...> + Send` instead of plain `async fn`? Both mean
/// "this function is async". Spelling out `+ Send` tells Rust the future may
/// move between threads, which Tokio needs to run it in the background.
/// Implementations can still be written as ordinary `async fn`.
///
/// Note: traits written this way cannot be used as `dyn Driver`. When the UI
/// needs to hold "some driver", we will use a small enum (Postgres | MySql)
/// instead; that keeps this code plain and easy to read.
pub trait Driver: Send + Sync {
    /// Check the connection works and report which server we are on.
    fn ping(&self) -> impl Future<Output = Result<ServerInfo, DriverError>> + Send;

    /// Run `sql` and stream its results into `tx` in batches.
    ///
    /// This runs the text exactly as given. Rowlark never rewrites or parses
    /// the user's SQL; the server decides whether it is allowed.
    fn run_query(
        &self,
        sql: &str,
        tx: mpsc::Sender<QueryEvent>,
    ) -> impl Future<Output = Result<QuerySummary, DriverError>> + Send;

    /// Stop the query that is running on this connection, on the server too.
    ///
    /// It takes `&self`, so it can be called from another task while
    /// `run_query` is still going (hold the driver in an `Arc` to share it).
    /// The stopped `run_query` returns `Err(DriverError::Cancelled)`.
    /// If nothing is running, this does nothing and returns `Ok`.
    fn cancel(&self) -> impl Future<Output = Result<(), DriverError>> + Send;
}
