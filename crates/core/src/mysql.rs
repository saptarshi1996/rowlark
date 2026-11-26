//! The MySQL and MariaDB driver, built on `sqlx`.
//!
//! One driver serves both servers: they speak the same protocol. On connect we
//! ask `SELECT VERSION()` and record which one we got (`Flavor`), so quirks can
//! be handled here and nowhere else.
//!
//! How values stay exact: queries run through sqlx's `raw_sql`, which uses
//! MySQL's plain text protocol. The server then sends every value as TEXT,
//! exactly as the `mysql` command line would print it (`0.10` stays `0.10`,
//! `0000-00-00` stays `0000-00-00`). We read that text and wrap it in a
//! `Value` based on the column's type name. We never convert through f64.
//!
//! Things we deliberately do NOT let sqlx do, because Rowlark runs the SQL the
//! user typed and must not change what it means (see `connect_options`).
//!
//! Status: connecting, `ping` and value conversion are done. Streaming here is
//! the basic version; cancel, notices, transactions and introspection come
//! next, and then these methods move into `impl Driver for MySqlDriver`.

use std::time::Duration;

use futures_util::StreamExt;
use sqlx::mysql::{
    MySqlConnectOptions, MySqlConnection, MySqlDatabaseError, MySqlRow, MySqlSslMode,
};
use sqlx::{Column as _, Connection, Either, Executor as _, Row, TypeInfo};
use tokio::sync::{Mutex, mpsc};

use crate::connection::{ConnectionConfig, TlsMode};
use crate::driver::{
    Column, DriverError, Flavor, QueryEvent, QuerySummary, ServerInfo, TransactionState,
};
use crate::value::Value;

/// How many rows we send to the UI at a time.
const BATCH_SIZE: usize = 1000;

/// One connection to a MySQL or MariaDB server.
///
/// As with Postgres, a dedicated connection per editor tab keeps transactions
/// and session settings separate, and the `Mutex` makes callers take turns.
pub struct MySqlDriver {
    conn: Mutex<MySqlConnection>,
    flavor: Flavor,
    /// The server's own version text, e.g. "8.0.39" or "11.4.2-MariaDB-ubu2404".
    version: String,
}

impl MySqlDriver {
    /// Open a connection. `password` comes from the OS keychain (see `ConnectionConfig`).
    pub async fn connect(config: &ConnectionConfig, password: &str) -> Result<Self, DriverError> {
        let options = connect_options(config, password)?;
        let mut conn = open_connection(&options, config.connect_timeout).await?;

        // We turned off sqlx's own `SET NAMES` (see `connect_options`), so say
        // what we want ourselves: UTF-8, with the server's own default
        // collation for it, exactly what the `mysql` command line ends up with.
        conn.execute("SET NAMES utf8mb4")
            .await
            .map_err(server_error)?;

        undo_ignore_space(&mut conn).await?;

        let version: String = sqlx::query_scalar("SELECT VERSION()")
            .fetch_one(&mut conn)
            .await
            .map_err(server_error)?;
        Ok(Self {
            conn: Mutex::new(conn),
            flavor: flavor_from_version(&version),
            version,
        })
    }

    /// Check the connection still answers, and report which server we are on.
    pub async fn ping(&self) -> Result<ServerInfo, DriverError> {
        let mut conn = self.conn.lock().await;
        // A real round trip, so a dead connection shows up here.
        conn.ping().await.map_err(server_error)?;
        Ok(ServerInfo {
            flavor: self.flavor,
            version: self.version.clone(),
        })
    }

    /// Whether a transaction is open on this connection.
    ///
    /// Asks the server, so it also notices a `BEGIN` the user typed and one
    /// that a statement like `CREATE TABLE` ended without being asked. Returns
    /// `Busy` straight away while a query is running.
    ///
    /// MySQL never has Postgres's "aborted" state: after an error the
    /// transaction stays open and usable, so `Aborted` is never returned.
    ///
    /// How it asks: MySQL has no variable that says "a transaction is open",
    /// and the tables that do need privileges an ordinary user lacks. What
    /// every user can do is this: `RELEASE SAVEPOINT` of a savepoint we just
    /// made works inside a transaction, and fails with error 1305 ("does not
    /// exist") outside one, because with autocommit on there is nothing for
    /// the savepoint to live in. The savepoint is gone again right after, and
    /// it has a name no one else would use. One known quirk, pinned by a test:
    /// with `autocommit` OFF the savepoint itself opens MySQL's implicit
    /// transaction, so the answer is `Open` even straight after a `COMMIT`.
    /// (Such a session opens one at its next statement anyway, and a bare
    /// savepoint holds no locks.)
    pub async fn transaction_state(&self) -> Result<TransactionState, DriverError> {
        // If a query holds the connection, we cannot ask. Do not wait for it.
        let Ok(mut conn) = self.conn.try_lock() else {
            return Ok(TransactionState::Busy);
        };
        let mut stream =
            sqlx::raw_sql("SAVEPOINT rowlark_tx_probe; RELEASE SAVEPOINT rowlark_tx_probe")
                .fetch_many(&mut *conn);
        while let Some(item) = stream.next().await {
            if let Err(error) = item {
                return if is_missing_savepoint(&error) {
                    Ok(TransactionState::Idle)
                } else {
                    Err(server_error(error))
                };
            }
        }
        Ok(TransactionState::Open)
    }

    /// Run `sql` and stream its results into `tx` in batches of up to 1,000
    /// rows. The text is sent exactly as given.
    pub async fn run_query(
        &self,
        sql: &str,
        tx: mpsc::Sender<QueryEvent>,
    ) -> Result<QuerySummary, DriverError> {
        let mut conn = self.conn.lock().await;
        // `fetch_many` yields two kinds of item, in order: rows (`Right`) and,
        // after each statement finishes, that statement's summary (`Left`).
        // The SQL may hold several statements, so there can be several result sets.
        let mut stream = sqlx::raw_sql(sql).fetch_many(&mut *conn);

        let mut summary = QuerySummary::default();
        let mut batch: Vec<Vec<Value>> = Vec::with_capacity(BATCH_SIZE);
        let mut columns_sent = false;
        // Type names of the current result, worked out once from its first row.
        let mut type_names: Vec<String> = Vec::new();
        let mut rows_in_statement: u64 = 0;

        while let Some(item) = stream.next().await {
            let item = match item {
                Ok(item) => item,
                Err(error) => {
                    // The statement failed partway. Rows already read are still
                    // good data the user should see, so send them first.
                    if !batch.is_empty() {
                        send(&tx, QueryEvent::Rows(std::mem::take(&mut batch))).await?;
                    }
                    return Err(server_error(error));
                }
            };
            match item {
                Either::Right(row) => {
                    if !columns_sent {
                        let columns = describe_columns(&row);
                        type_names = columns.iter().map(|c| c.type_name.clone()).collect();
                        send(&tx, QueryEvent::Columns(columns)).await?;
                        columns_sent = true;
                    }
                    batch.push(convert_row(&row, &type_names));
                    rows_in_statement += 1;
                    summary.rows_returned += 1;
                    if batch.len() >= BATCH_SIZE {
                        send(&tx, QueryEvent::Rows(std::mem::take(&mut batch))).await?;
                    }
                }
                Either::Left(done) => {
                    // The statement is finished: flush what is left of its rows.
                    if !batch.is_empty() {
                        send(&tx, QueryEvent::Rows(std::mem::take(&mut batch))).await?;
                    }
                    // Only statements that returned no rows (INSERT, UPDATE, ...)
                    // count as "affected".
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
}

/// Is this MySQL's error 1305, "SAVEPOINT ... does not exist"? We look at the
/// error *number*, not the SQLSTATE: its SQLSTATE (42000) is shared with
/// ordinary syntax errors, so it cannot tell them apart.
fn is_missing_savepoint(error: &sqlx::Error) -> bool {
    let sqlx::Error::Database(db) = error else {
        return false;
    };
    db.try_downcast_ref::<MySqlDatabaseError>()
        .is_some_and(|e| e.number() == 1305)
}

/// sqlx always tells the server "ignore space after function names" (the
/// `IGNORE_SPACE` client flag), and the server answers by adding `IGNORE_SPACE`
/// to this session's `sql_mode`. That changes how SQL is read: every built-in
/// function name becomes a reserved word, so a table or column called `count`
/// or `date` stops working. The `mysql` command line does not do this, and
/// sqlx offers no switch, so we take that one mode back out, unless the
/// server's own default has it (then it is what the user's server expects).
async fn undo_ignore_space(conn: &mut MySqlConnection) -> Result<(), DriverError> {
    let (session, global): (String, String) =
        sqlx::query_as("SELECT @@SESSION.sql_mode, @@GLOBAL.sql_mode")
            .fetch_one(&mut *conn)
            .await
            .map_err(server_error)?;
    if let Some(mode) = sql_mode_without_ignore_space(&session, &global) {
        // The text is only mode names (checked in the function above), so it
        // is safe to put between quotes.
        conn.execute(format!("SET SESSION sql_mode = '{mode}'").as_str())
            .await
            .map_err(server_error)?;
    }
    Ok(())
}

/// The session's `sql_mode` without `IGNORE_SPACE`, or `None` when nothing
/// needs to change: it is not there, or the server's global default has it
/// too, or the text holds anything but mode names (then we leave it alone
/// rather than build SQL from it).
fn sql_mode_without_ignore_space(session: &str, global: &str) -> Option<String> {
    let has = |modes: &str| modes.split(',').any(|m| m == "IGNORE_SPACE");
    let only_mode_names = session
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == ',');
    if !has(session) || has(global) || !only_mode_names {
        return None;
    }
    Some(
        session
            .split(',')
            .filter(|m| *m != "IGNORE_SPACE")
            .collect::<Vec<_>>()
            .join(","),
    )
}

/// MariaDB says so in its version text ("11.4.2-MariaDB-ubu2404", or
/// "5.5.5-10.11.6-MariaDB" from older protocol setups); MySQL's is plain
/// ("8.0.39", "9.1.0"). Some MySQL-compatible servers do not say either way,
/// and those are treated as MySQL.
fn flavor_from_version(version: &str) -> Flavor {
    if version.to_ascii_lowercase().contains("mariadb") {
        Flavor::MariaDb
    } else {
        Flavor::MySql
    }
}

/// Translate our connection settings into sqlx's.
///
/// sqlx quietly changes a few session settings to suit its own needs. A
/// database client that runs the user's SQL must not, so we switch them off:
/// - `PIPES_AS_CONCAT` would turn `a || b` from "or" into "join text";
/// - `NO_ENGINE_SUBSTITUTION` is added to `sql_mode` (changes what `CREATE
///   TABLE ... ENGINE=x` does when x is missing);
/// - `time_zone = '+00:00'` would show every TIMESTAMP in UTC instead of the
///   server's own zone, unlike the `mysql` command line.
fn connect_options(
    config: &ConnectionConfig,
    password: &str,
) -> Result<MySqlConnectOptions, DriverError> {
    let tls = &config.tls;
    let mut options = MySqlConnectOptions::new()
        .host(&config.host)
        .port(config.port)
        .username(&config.user)
        .password(password)
        .database(&config.database)
        .charset("utf8mb4")
        // We run `SET NAMES utf8mb4` ourselves in `connect`, without sqlx's
        // fixed collation.
        .set_names(false)
        .pipes_as_concat(false)
        .no_engine_substitution(false)
        .timezone(None)
        .ssl_mode(match tls.mode {
            TlsMode::Disable => MySqlSslMode::Disabled,
            TlsMode::Prefer => MySqlSslMode::Preferred,
            TlsMode::Require => MySqlSslMode::Required,
            TlsMode::VerifyCa => MySqlSslMode::VerifyCa,
            TlsMode::VerifyFull => MySqlSslMode::VerifyIdentity,
        });
    // Fail early and clearly if a certificate file is missing. Otherwise the
    // problem would only show up later as a vague handshake error.
    for (label, path) in [
        ("CA certificate", &tls.ca_cert),
        ("client certificate", &tls.client_cert),
        ("client key", &tls.client_key),
    ] {
        if let Some(path) = path
            && !path.is_file()
        {
            return Err(DriverError::InvalidConfig(format!(
                "the {label} file {} does not exist",
                path.display()
            )));
        }
    }
    if let Some(path) = &tls.ca_cert {
        options = options.ssl_ca(path);
    }
    if let Some(path) = &tls.client_cert {
        options = options.ssl_client_cert(path);
    }
    if let Some(path) = &tls.client_key {
        options = options.ssl_client_key(path);
    }
    Ok(options)
}

/// Connect, giving up after `timeout` instead of hanging on an unreachable host.
async fn open_connection(
    options: &MySqlConnectOptions,
    timeout: Duration,
) -> Result<MySqlConnection, DriverError> {
    match tokio::time::timeout(timeout, MySqlConnection::connect_with(options)).await {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(error)) => Err(DriverError::Connection(error.to_string())),
        Err(_) => Err(DriverError::Connection(format!(
            "no answer from the server after {} seconds",
            timeout.as_secs()
        ))),
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
        sqlx::Error::Database(db) => DriverError::Server {
            code: db.code().map(|c| c.to_string()),
            message: db.message().to_string(),
        },
        other => DriverError::Connection(other.to_string()),
    }
}

/// The name and type of each column of a result.
fn describe_columns(row: &MySqlRow) -> Vec<Column> {
    row.columns()
        .iter()
        .map(|c| Column {
            name: c.name().to_string(),
            type_name: c.type_info().name().to_string(),
            // Tracing a column back to its table comes later.
            origin: None,
        })
        .collect()
}

/// Convert one row. `type_names` holds each column's type name (same order).
fn convert_row(row: &MySqlRow, type_names: &[String]) -> Vec<Value> {
    type_names
        .iter()
        .enumerate()
        .map(|(i, type_name)| convert_cell(row, i, type_name))
        .collect()
}

/// Read one cell's raw bytes and wrap them in the right `Value`.
fn convert_cell(row: &MySqlRow, index: usize, type_name: &str) -> Value {
    // We read the bytes unchecked (sqlx would otherwise insist we name the
    // exact Rust type for the column, which is the opposite of what we want).
    // `Option` turns SQL NULL into `None`. An error here can only mean an
    // out-of-range index, which cannot happen because we loop over the row's
    // own length; treat it as NULL to stay safe.
    match row.try_get_unchecked::<Option<Vec<u8>>, _>(index) {
        Ok(Some(bytes)) => bytes_to_value(type_name, bytes),
        Ok(None) | Err(_) => Value::Null,
    }
}

/// Types that are always bytes. (`BIT` and `GEOMETRY` are sent as raw bytes
/// even though they are not "binary strings" in SQL.)
fn is_raw_bytes_type(type_name: &str) -> bool {
    matches!(type_name, "BIT" | "GEOMETRY")
}

/// Types sqlx names after the server's BINARY flag. That flag does not only
/// mean "binary string": MySQL also sets it on text with a `_bin` collation
/// (`VARCHAR(20) COLLATE utf8mb4_bin`), and MariaDB's `JSON` is exactly such a
/// `LONGTEXT`. sqlx does not pass on the character set that would tell the
/// two apart, so we cannot know from the column alone.
fn is_binary_string_type(type_name: &str) -> bool {
    matches!(
        type_name,
        "BINARY" | "VARBINARY" | "TINYBLOB" | "BLOB" | "MEDIUMBLOB" | "LONGBLOB"
    )
}

/// Decide by the value instead: bytes that are valid UTF-8 and contain no
/// control characters (other than tab and line breaks) are text. Real binary
/// data (images, hashes, UUIDs, zero-padded `BINARY(n)`) almost always breaks
/// one of those rules and stays bytes.
///
/// The known cost: a blob that happens to be plain readable text, like
/// `hello`, shows as text. The column's type name still says BLOB, and
/// editing it sends the same bytes back, so nothing is lost.
fn looks_like_text(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    let readable = text
        .chars()
        .all(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r'));
    readable.then_some(text)
}

/// Map a MySQL type name and the value's bytes to a `Value`.
///
/// Pure (no database needed), so it is easy to test. Anything we do not
/// recognise, or fail to parse, becomes `Unknown` with the server's text, so
/// the user still sees the data.
fn bytes_to_value(type_name: &str, bytes: Vec<u8>) -> Value {
    if is_raw_bytes_type(type_name) {
        return Value::Bytes(bytes);
    }
    if is_binary_string_type(type_name) {
        return match looks_like_text(&bytes) {
            Some(text) => Value::Text(text.to_string()),
            None => Value::Bytes(bytes),
        };
    }
    // We ask the server for UTF-8, so text normally is. If some column still
    // sends other bytes (a legacy charset forced by a view, a function that
    // returns binary), hand over the exact bytes rather than corrupt them.
    match String::from_utf8(bytes) {
        Ok(text) => text_to_value(type_name, &text),
        Err(error) => Value::Bytes(error.into_bytes()),
    }
}

fn text_to_value(type_name: &str, text: &str) -> Value {
    let unknown = || Value::Unknown {
        type_name: type_name.to_string(),
        text: text.to_string(),
    };
    // sqlx adds " UNSIGNED" to integer type names. The values can exceed i64.
    if let Some(signed) = type_name.strip_suffix(" UNSIGNED") {
        return match signed {
            "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" => {
                text.parse().map(Value::UInt).unwrap_or_else(|_| unknown())
            }
            _ => unknown(),
        };
    }
    match type_name {
        // sqlx calls a one-digit-wide TINYINT "BOOLEAN" (that is all MySQL's
        // BOOLEAN is). The column can still hold any small number, so only 0
        // and 1 become a bool.
        "BOOLEAN" => match text {
            "0" => Value::Bool(false),
            "1" => Value::Bool(true),
            _ => text.parse().map(Value::Int).unwrap_or_else(|_| unknown()),
        },
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" => {
            text.parse().map(Value::Int).unwrap_or_else(|_| unknown())
        }
        "FLOAT" | "DOUBLE" => text.parse().map(Value::Float).unwrap_or_else(|_| unknown()),
        // Decimals stay as the exact text, never a float.
        "DECIMAL" => Value::Decimal(text.to_string()),
        "CHAR" | "VARCHAR" | "TINYTEXT" | "TEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM" | "SET" => {
            Value::Text(text.to_string())
        }
        "JSON" => Value::Json(text.to_string()),
        // Dates and times keep the server's text, including values MySQL
        // allows but most languages do not (`0000-00-00`, `838:59:59`).
        // The server's time zone is not sent, so `tz` is unknown.
        "DATETIME" | "TIMESTAMP" => Value::Timestamp {
            iso: text.to_string(),
            tz: None,
        },
        "DATE" => Value::Date(text.to_string()),
        "TIME" => Value::Time(text.to_string()),
        // `YEAR` can be `0000`, which a number would turn into `0`; and
        // writing `0` back means the year 2000. Keep the text.
        _ => unknown(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::TlsConfig;

    fn text(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    #[test]
    fn text_in_a_bin_collation_is_text_not_bytes() {
        // MySQL flags `VARCHAR ... COLLATE utf8mb4_bin` as binary, so sqlx
        // calls it VARBINARY. People store usernames, codes and MariaDB JSON
        // like this; they must read as text.
        assert_eq!(
            bytes_to_value("VARBINARY", text("aB é 😀")),
            Value::Text("aB é 😀".into())
        );
        assert_eq!(
            bytes_to_value("BLOB", text("{\"b\": 1,  \"a\":2}")),
            Value::Text("{\"b\": 1,  \"a\":2}".into())
        );
        // Tabs and line breaks are ordinary text.
        assert_eq!(
            bytes_to_value("LONGBLOB", text("a\tb\r\nc")),
            Value::Text("a\tb\r\nc".into())
        );
    }

    #[test]
    fn real_binary_data_stays_bytes() {
        // Not UTF-8.
        assert_eq!(
            bytes_to_value("VARBINARY", vec![0xff, 0xfe]),
            Value::Bytes(vec![0xff, 0xfe])
        );
        // Valid UTF-8 but clearly not text: control characters and NULs
        // (BINARY(n) pads with zero bytes).
        assert_eq!(
            bytes_to_value("BINARY", vec![b'a', 0, 0]),
            Value::Bytes(vec![b'a', 0, 0])
        );
        assert_eq!(
            bytes_to_value("BLOB", vec![0x01, 0x02]),
            Value::Bytes(vec![0x01, 0x02])
        );
        // BIT and GEOMETRY are always bytes, even when they look like text.
        assert_eq!(bytes_to_value("BIT", vec![0x41]), Value::Bytes(vec![0x41]));
        assert_eq!(
            bytes_to_value("GEOMETRY", text("abc")),
            Value::Bytes(text("abc"))
        );
    }

    #[test]
    fn integers_keep_all_their_bits() {
        assert_eq!(
            text_to_value("BIGINT", "-9223372036854775808"),
            Value::Int(i64::MIN)
        );
        assert_eq!(text_to_value("MEDIUMINT", "-5"), Value::Int(-5));
        // Unsigned values can be larger than an i64.
        assert_eq!(
            text_to_value("BIGINT UNSIGNED", "18446744073709551615"),
            Value::UInt(u64::MAX)
        );
        assert_eq!(text_to_value("TINYINT UNSIGNED", "255"), Value::UInt(255));
        // Text we cannot read as a number is shown, not hidden.
        assert!(matches!(
            text_to_value("INT", "many"),
            Value::Unknown { .. }
        ));
        assert!(matches!(
            text_to_value("BIGINT UNSIGNED", "-1"),
            Value::Unknown { .. }
        ));
        assert!(matches!(
            text_to_value("DECIMAL UNSIGNED", "1"),
            Value::Unknown { .. }
        ));
    }

    #[test]
    fn a_one_digit_tinyint_is_a_bool_only_for_zero_and_one() {
        assert_eq!(text_to_value("BOOLEAN", "1"), Value::Bool(true));
        assert_eq!(text_to_value("BOOLEAN", "0"), Value::Bool(false));
        // TINYINT(1) can hold other numbers; do not call them true.
        assert_eq!(text_to_value("BOOLEAN", "5"), Value::Int(5));
        assert_eq!(text_to_value("BOOLEAN", "-1"), Value::Int(-1));
        assert!(matches!(
            text_to_value("BOOLEAN", "x"),
            Value::Unknown { .. }
        ));
    }

    #[test]
    fn decimals_and_dates_keep_the_servers_exact_text() {
        assert_eq!(
            text_to_value("DECIMAL", "0.10"),
            Value::Decimal("0.10".into())
        );
        assert_eq!(
            text_to_value("DECIMAL", "-12345678901234567890.123456789"),
            Value::Decimal("-12345678901234567890.123456789".into())
        );
        assert_eq!(
            text_to_value("DATE", "0000-00-00"),
            Value::Date("0000-00-00".into())
        );
        assert_eq!(
            text_to_value("TIME", "-838:59:59.000000"),
            Value::Time("-838:59:59.000000".into())
        );
        for name in ["DATETIME", "TIMESTAMP"] {
            assert_eq!(
                text_to_value(name, "0000-00-00 00:00:00"),
                Value::Timestamp {
                    iso: "0000-00-00 00:00:00".into(),
                    tz: None
                }
            );
        }
        assert_eq!(
            text_to_value("JSON", "{\"b\": 1}"),
            Value::Json("{\"b\": 1}".into())
        );
    }

    #[test]
    fn floats_parse_and_other_types_fall_back_to_exact_text() {
        assert_eq!(text_to_value("DOUBLE", "1.5"), Value::Float(1.5));
        assert_eq!(
            text_to_value("FLOAT", "-3.40282e38"),
            Value::Float(-3.40282e38)
        );
        assert!(matches!(
            text_to_value("DOUBLE", "nope"),
            Value::Unknown { .. }
        ));
        // YEAR keeps its text: "0000" as a number would become 0, and 0 is
        // read back as the year 2000.
        assert_eq!(
            text_to_value("YEAR", "0000"),
            Value::Unknown {
                type_name: "YEAR".into(),
                text: "0000".into()
            }
        );
        for name in [
            "ENUM",
            "SET",
            "CHAR",
            "VARCHAR",
            "TINYTEXT",
            "TEXT",
            "MEDIUMTEXT",
            "LONGTEXT",
        ] {
            assert_eq!(
                text_to_value(name, "a,b"),
                Value::Text("a,b".into()),
                "{name}"
            );
        }
    }

    #[test]
    fn text_that_is_not_utf8_is_kept_as_exact_bytes() {
        assert_eq!(
            bytes_to_value("VARCHAR", vec![b'a', 0xe9]),
            Value::Bytes(vec![b'a', 0xe9])
        );
    }

    #[test]
    fn ignore_space_is_removed_only_when_the_server_did_not_ask_for_it() {
        let global = "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES";
        // The usual case: sqlx's flag put it first.
        assert_eq!(
            sql_mode_without_ignore_space(
                "IGNORE_SPACE,ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES",
                global
            ),
            Some(global.to_string())
        );
        // Wherever it sits, and when it is the only mode.
        assert_eq!(
            sql_mode_without_ignore_space("A,IGNORE_SPACE,B", "A,B"),
            Some("A,B".to_string())
        );
        assert_eq!(
            sql_mode_without_ignore_space("IGNORE_SPACE", ""),
            Some(String::new())
        );
        // Not there: nothing to do. (A mode that merely contains the word is
        // a different mode.)
        assert_eq!(sql_mode_without_ignore_space(global, global), None);
        assert_eq!(sql_mode_without_ignore_space("NOT_IGNORE_SPACE", ""), None);
        // The server's own default has it: respect that.
        assert_eq!(
            sql_mode_without_ignore_space("IGNORE_SPACE,A", "IGNORE_SPACE,A"),
            None
        );
        // Anything that is not plain mode names is left alone.
        assert_eq!(
            sql_mode_without_ignore_space("IGNORE_SPACE,A'; DROP TABLE x; --", ""),
            None
        );
    }

    #[test]
    fn the_flavor_comes_from_the_version_text() {
        assert_eq!(flavor_from_version("8.0.46"), Flavor::MySql);
        assert_eq!(flavor_from_version("9.7.4"), Flavor::MySql);
        assert_eq!(
            flavor_from_version("8.0.46-0ubuntu0.22.04.1"),
            Flavor::MySql
        );
        assert_eq!(
            flavor_from_version("11.8.9-MariaDB-ubu2404"),
            Flavor::MariaDb
        );
        assert_eq!(
            flavor_from_version("5.5.5-10.11.6-MariaDB"),
            Flavor::MariaDb
        );
        assert_eq!(flavor_from_version("10.6.0-mariadb-log"), Flavor::MariaDb);
    }

    #[test]
    fn connection_settings_do_not_change_what_the_users_sql_means() {
        let config = ConnectionConfig::new("db.example.com", 3307, "me", "shop");
        let options = connect_options(&config, "pw").expect("valid settings");
        assert_eq!(options.get_host(), "db.example.com");
        assert_eq!(options.get_port(), 3307);
        assert_eq!(options.get_username(), "me");
        assert_eq!(options.get_database(), Some("shop"));
        assert_eq!(options.get_charset(), "utf8mb4");
        // The three session changes sqlx makes by default are off; the
        // database tests check the server really is left alone.
        let debug = format!("{options:?}");
        assert!(debug.contains("pipes_as_concat: false"), "{debug}");
        assert!(debug.contains("no_engine_substitution: false"), "{debug}");
        assert!(debug.contains("timezone: None"), "{debug}");
        assert!(debug.contains("set_names: false"), "{debug}");
    }

    #[test]
    fn every_tls_mode_maps_to_the_matching_mysql_mode() {
        let mode = |tls: TlsMode| {
            let mut config = ConnectionConfig::new("h", 3306, "u", "d");
            config.tls = TlsConfig::new(tls);
            format!(
                "{:?}",
                connect_options(&config, "pw")
                    .expect("valid")
                    .get_ssl_mode()
            )
        };
        assert_eq!(mode(TlsMode::Disable), "Disabled");
        assert_eq!(mode(TlsMode::Prefer), "Preferred");
        assert_eq!(mode(TlsMode::Require), "Required");
        assert_eq!(mode(TlsMode::VerifyCa), "VerifyCa");
        assert_eq!(mode(TlsMode::VerifyFull), "VerifyIdentity");
    }

    #[test]
    fn a_missing_certificate_file_is_a_clear_error() {
        let missing = std::path::PathBuf::from("/no/such/file.pem");
        for (label, tls) in [
            (
                "CA certificate",
                TlsConfig {
                    ca_cert: Some(missing.clone()),
                    ..TlsConfig::new(TlsMode::VerifyFull)
                },
            ),
            (
                "client certificate",
                TlsConfig {
                    client_cert: Some(missing.clone()),
                    ..TlsConfig::new(TlsMode::VerifyFull)
                },
            ),
            (
                "client key",
                TlsConfig {
                    client_key: Some(missing.clone()),
                    ..TlsConfig::new(TlsMode::VerifyFull)
                },
            ),
        ] {
            let mut config = ConnectionConfig::new("h", 3306, "u", "d");
            config.tls = tls;
            match connect_options(&config, "pw") {
                Err(DriverError::InvalidConfig(message)) => {
                    assert!(
                        message.contains(label) && message.contains("/no/such/file.pem"),
                        "{message}"
                    );
                }
                other => panic!("{label}: {:?}", other.err()),
            }
        }
    }

    #[test]
    fn existing_certificate_files_are_accepted() {
        // Any file that exists will do here; its contents are read later, at
        // connect time.
        let here = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let mut config = ConnectionConfig::new("h", 3306, "u", "d");
        config.tls = TlsConfig {
            ca_cert: Some(here.clone()),
            client_cert: Some(here.clone()),
            client_key: Some(here),
            ..TlsConfig::new(TlsMode::VerifyFull)
        };
        assert!(connect_options(&config, "pw").is_ok());
    }
}
