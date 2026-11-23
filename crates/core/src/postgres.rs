//! The PostgreSQL driver, built on `sqlx`.
//!
//! How values stay exact: we run queries with sqlx's `raw_sql`, which uses
//! Postgres's "simple query" protocol. In that protocol the server sends every
//! value as TEXT, exactly as `psql` would print it (`0.10` stays `0.10`, a
//! timestamp keeps its offset). We read that text and wrap it in a `Value`
//! based on the column's type name. We never convert through f64.
//!
//! The trade-off: the simple protocol cannot take parameters. That is fine
//! here, because we run the SQL the user typed, as typed.

use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::{Stream, StreamExt};
use sqlx::postgres::{
    PgConnectOptions, PgConnection, PgQueryResult, PgRow, PgSslMode, PgTypeInfo, PgValueFormat,
};
use sqlx::{Column as _, Connection, Either, Row, TypeInfo, ValueRef};
use tokio::sync::{Mutex, mpsc};

use crate::driver::{
    Column, ConnectionConfig, Driver, DriverError, Flavor, QueryEvent, QuerySummary, ServerInfo,
};
use crate::value::Value;

/// How many rows we send to the UI at a time.
const BATCH_SIZE: usize = 1000;

/// One connection to a Postgres server.
///
/// A dedicated connection per editor tab keeps transactions and session
/// settings separate between tabs. The `Mutex` makes sure only one query uses
/// the connection at a time; a second caller waits its turn.
pub struct PostgresDriver {
    conn: Mutex<PgConnection>,
    // The fields below live outside the Mutex on purpose. `run_query` holds the
    // Mutex for the whole query, and `cancel` must work *during* that time.
    /// How to open a second connection (for cancel). Holds the password in
    /// memory only; this struct has no `Debug`, so it cannot be printed.
    options: PgConnectOptions,
    /// The server's id for our main connection, as used by `pg_cancel_backend`.
    backend_pid: i32,
    /// True while a query is running, so `cancel` can skip when idle.
    busy: AtomicBool,
}

impl PostgresDriver {
    /// Open a connection. `password` comes from the OS keychain (see `ConnectionConfig`).
    pub async fn connect(config: &ConnectionConfig, password: &str) -> Result<Self, DriverError> {
        let options = PgConnectOptions::new()
            .host(&config.host)
            .port(config.port)
            .username(&config.user)
            .password(password)
            .database(&config.database)
            // TLS support arrives in Phase 1b. Local and Docker servers do not need it.
            .ssl_mode(PgSslMode::Disable);

        let mut conn = PgConnection::connect_with(&options)
            .await
            .map_err(|e| DriverError::Connection(e.to_string()))?;
        // Remember this connection's id on the server. Cancel needs it later,
        // when the connection itself is too busy to answer.
        let backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut conn)
            .await
            .map_err(server_error)?;
        Ok(Self {
            conn: Mutex::new(conn),
            options,
            backend_pid,
            busy: AtomicBool::new(false),
        })
    }

    /// The real work of `run_query`, split out so the wrapper can reset `busy`
    /// no matter how this function ends.
    async fn stream_query(
        &self,
        sql: &str,
        tx: &mpsc::Sender<QueryEvent>,
    ) -> Result<QuerySummary, DriverError> {
        let mut conn = self.conn.lock().await;
        self.busy.store(true, Ordering::SeqCst);
        // `fetch_many` yields two kinds of item, in order: rows (`Right`) and,
        // after each statement finishes, that statement's summary (`Left`).
        // The SQL may hold several statements, so there can be several result sets.
        let mut stream = sqlx::raw_sql(sql).fetch_many(&mut *conn);

        let outcome = pump(&mut stream, tx).await;
        if matches!(outcome, Err(DriverError::ReceiverClosed)) {
            // The UI went away mid-query, but the server is still working and
            // still sending rows. Tell it to stop, then read and throw away
            // whatever is left. That last step matters: if we dropped the
            // stream half-read, the server's "cancelled" error would stay on
            // the connection and make the NEXT query fail by mistake.
            // Best effort: we still report the original `ReceiverClosed`.
            let _ = self.cancel().await;
            while stream.next().await.is_some() {}
        }
        outcome
    }
}

/// Read rows from the server and forward them to the UI in batches.
///
/// `S` can be any stream of sqlx results; `Unpin` just means it may be
/// polled by reference (`&mut`), which `stream.next()` needs.
async fn pump<S>(stream: &mut S, tx: &mpsc::Sender<QueryEvent>) -> Result<QuerySummary, DriverError>
where
    S: Stream<Item = Result<Either<PgQueryResult, PgRow>, sqlx::Error>> + Unpin,
{
    let mut summary = QuerySummary::default();
    let mut batch: Vec<Vec<Value>> = Vec::with_capacity(BATCH_SIZE);
    let mut columns_sent = false;
    let mut rows_in_statement: u64 = 0;

    while let Some(item) = stream.next().await {
        match item.map_err(server_error)? {
            Either::Right(row) => {
                if !columns_sent {
                    send(tx, QueryEvent::Columns(columns_of(&row))).await?;
                    columns_sent = true;
                }
                batch.push(convert_row(&row));
                rows_in_statement += 1;
                summary.rows_returned += 1;
                if batch.len() >= BATCH_SIZE {
                    send(tx, QueryEvent::Rows(std::mem::take(&mut batch))).await?;
                }
            }
            Either::Left(done) => {
                // The statement is finished: flush what is left of its rows.
                if !batch.is_empty() {
                    send(tx, QueryEvent::Rows(std::mem::take(&mut batch))).await?;
                }
                // For a SELECT the server also reports "N rows", which we
                // already counted as returned. Only count it as "affected"
                // for statements that returned no rows (INSERT, UPDATE, ...).
                if rows_in_statement == 0 {
                    summary.rows_affected += done.rows_affected();
                }
                columns_sent = false;
                rows_in_statement = 0;
            }
        }
    }
    Ok(summary)
}

impl Driver for PostgresDriver {
    async fn ping(&self) -> Result<ServerInfo, DriverError> {
        let mut conn = self.conn.lock().await;
        let version: String = sqlx::query_scalar("SELECT version()")
            .fetch_one(&mut *conn)
            .await
            .map_err(server_error)?;
        Ok(ServerInfo {
            flavor: Flavor::Postgres,
            version,
        })
    }

    async fn run_query(
        &self,
        sql: &str,
        tx: mpsc::Sender<QueryEvent>,
    ) -> Result<QuerySummary, DriverError> {
        let result = self.stream_query(sql, &tx).await;
        self.busy.store(false, Ordering::SeqCst);
        result
    }

    async fn cancel(&self) -> Result<(), DriverError> {
        if !self.busy.load(Ordering::SeqCst) {
            // Nothing is running. Sending a cancel now could hit the *next*
            // query instead (Postgres cancels "whatever runs right now").
            return Ok(());
        }
        // The main connection is busy, so open a second one just for this.
        let mut other = PgConnection::connect_with(&self.options)
            .await
            .map_err(|e| DriverError::Connection(e.to_string()))?;
        sqlx::query("SELECT pg_cancel_backend($1)")
            .bind(self.backend_pid)
            .execute(&mut other)
            .await
            .map_err(server_error)?;
        // Closing politely is optional; the server notices when we drop it.
        let _ = other.close().await;
        Ok(())
    }
}

/// Send an event to the UI. If the UI has gone away, stop the query.
async fn send(tx: &mpsc::Sender<QueryEvent>, event: QueryEvent) -> Result<(), DriverError> {
    tx.send(event)
        .await
        .map_err(|_| DriverError::ReceiverClosed)
}

/// Turn an sqlx error into ours, keeping the server's own words and SQLSTATE.
fn server_error(error: sqlx::Error) -> DriverError {
    match error {
        sqlx::Error::Database(db) => {
            let code = db.code().map(|c| c.to_string());
            // 57014 is Postgres's "query_canceled": the answer to our cancel.
            if code.as_deref() == Some("57014") {
                return DriverError::Cancelled;
            }
            DriverError::Server {
                code,
                message: db.message().to_string(),
            }
        }
        other => DriverError::Connection(other.to_string()),
    }
}

fn columns_of(row: &PgRow) -> Vec<Column> {
    row.columns()
        .iter()
        .map(|c| Column {
            name: c.name().to_string(),
            type_name: type_label(c.type_info()),
        })
        .collect()
}

/// The type's name, e.g. "INT4". For types sqlx does not know by name (enums,
/// domains and other custom types) it reports "?", because the server only
/// sends a type number (OID) and looking up its name would need another query
/// on the busy connection. Show the number instead, which at least tells
/// different custom types apart. (Resolving real names is a later improvement.)
fn type_label(info: &PgTypeInfo) -> String {
    let name = info.name();
    if name != "?" {
        return name.to_string();
    }
    match info.oid() {
        Some(oid) => format!("oid:{}", oid.0),
        None => "unknown".to_string(),
    }
}

fn convert_row(row: &PgRow) -> Vec<Value> {
    (0..row.len()).map(|i| convert_cell(row, i)).collect()
}

/// Read one cell as the server's text and wrap it in the right `Value`.
fn convert_cell(row: &PgRow, index: usize) -> Value {
    let type_name = type_label(row.column(index).type_info());
    let type_name = type_name.as_str();
    // `try_get_raw` only fails for an out-of-range index, which cannot happen
    // here (we loop over the row's own length). Treat it as NULL to stay safe.
    let Ok(raw) = row.try_get_raw(index) else {
        return Value::Null;
    };
    if raw.is_null() {
        return Value::Null;
    }
    let Ok(bytes) = raw.as_bytes() else {
        return Value::Null;
    };
    // The simple query protocol always sends text. If that ever changed, show
    // the type instead of guessing at binary bytes.
    if raw.format() != PgValueFormat::Text {
        return Value::Unknown {
            type_name: type_name.to_string(),
            text: String::new(),
        };
    }
    // Postgres text output is UTF-8 when the database encoding is UTF-8 (the default).
    let text = String::from_utf8_lossy(bytes);
    text_to_value(type_name, &text)
}

/// Map a Postgres type name and its text form to a `Value`.
///
/// This is a pure function (no database needed), so it is easy to test.
/// Anything we do not recognise, or fail to parse, becomes `Unknown` with the
/// server's text, so the user still sees the data.
fn text_to_value(type_name: &str, text: &str) -> Value {
    let unknown = || Value::Unknown {
        type_name: type_name.to_string(),
        text: text.to_string(),
    };
    match type_name {
        "BOOL" => match text {
            "t" => Value::Bool(true),
            "f" => Value::Bool(false),
            _ => unknown(),
        },
        "INT2" | "INT4" | "INT8" => text.parse().map(Value::Int).unwrap_or_else(|_| unknown()),
        // OID is an unsigned 32-bit number.
        "OID" => text.parse().map(Value::UInt).unwrap_or_else(|_| unknown()),
        // Rust's float parser understands NaN, Infinity and -Infinity.
        "FLOAT4" | "FLOAT8" => text.parse().map(Value::Float).unwrap_or_else(|_| unknown()),
        // Decimals stay as the exact text, never a float.
        "NUMERIC" => Value::Decimal(text.to_string()),
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "CHAR" => Value::Text(text.to_string()),
        "JSON" | "JSONB" => Value::Json(text.to_string()),
        "BYTEA" => decode_bytea(text).map(Value::Bytes).unwrap_or_else(unknown),
        // Timestamps keep the server's text, including the UTC offset for TIMESTAMPTZ.
        "TIMESTAMP" | "TIMESTAMPTZ" => Value::Timestamp {
            iso: text.to_string(),
            tz: None,
        },
        "DATE" => Value::Date(text.to_string()),
        "TIME" | "TIMETZ" => Value::Time(text.to_string()),
        "UUID" => Value::Uuid(text.to_string()),
        _ => unknown(),
    }
}

/// Postgres prints bytea as `\x` followed by hex digits (the default `hex` output).
fn decode_bytea(text: &str) -> Option<Vec<u8>> {
    let hex = text.strip_prefix("\\x")?;
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_parse_and_keep_all_64_bits() {
        assert_eq!(text_to_value("INT4", "-42"), Value::Int(-42));
        assert_eq!(
            text_to_value("INT8", "9223372036854775807"),
            Value::Int(i64::MAX)
        );
    }

    #[test]
    fn decimals_keep_their_exact_text() {
        assert_eq!(
            text_to_value("NUMERIC", "0.10"),
            Value::Decimal("0.10".into())
        );
        assert_eq!(
            text_to_value("NUMERIC", "12345678901234567890.123456789"),
            Value::Decimal("12345678901234567890.123456789".into())
        );
    }

    #[test]
    fn bools_use_postgres_t_and_f() {
        assert_eq!(text_to_value("BOOL", "t"), Value::Bool(true));
        assert_eq!(text_to_value("BOOL", "f"), Value::Bool(false));
    }

    #[test]
    fn special_floats_parse() {
        assert!(matches!(text_to_value("FLOAT8", "NaN"), Value::Float(f) if f.is_nan()));
        assert_eq!(
            text_to_value("FLOAT8", "Infinity"),
            Value::Float(f64::INFINITY)
        );
        assert_eq!(
            text_to_value("FLOAT8", "-Infinity"),
            Value::Float(f64::NEG_INFINITY)
        );
    }

    #[test]
    fn timestamps_keep_the_server_text() {
        assert_eq!(
            text_to_value("TIMESTAMPTZ", "2026-03-29 02:30:00+01"),
            Value::Timestamp {
                iso: "2026-03-29 02:30:00+01".into(),
                tz: None
            }
        );
    }

    #[test]
    fn json_is_kept_verbatim() {
        let original = r#"{"b": 1, "a": 1.10}"#;
        assert_eq!(
            text_to_value("JSONB", original),
            Value::Json(original.into())
        );
    }

    #[test]
    fn bytea_decodes_hex() {
        assert_eq!(
            text_to_value("BYTEA", "\\xdead000f"),
            Value::Bytes(vec![0xde, 0xad, 0x00, 0x0f])
        );
        assert_eq!(text_to_value("BYTEA", "\\x"), Value::Bytes(vec![]));
    }

    #[test]
    fn unknown_types_and_bad_input_fall_back_to_unknown() {
        assert_eq!(
            text_to_value("mood", "happy"),
            Value::Unknown {
                type_name: "mood".into(),
                text: "happy".into()
            }
        );
        assert!(matches!(
            text_to_value("INT4", "not a number"),
            Value::Unknown { .. }
        ));
    }
}
