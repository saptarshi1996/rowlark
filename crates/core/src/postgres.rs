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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex as StdMutex, PoisonError};
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use sqlx::postgres::{
    PgColumn, PgConnectOptions, PgConnection, PgQueryResult, PgRow, PgSslMode, PgValueFormat,
};
use sqlx::{Column as _, Connection, Either, Executor as _, Row, TypeInfo, ValueRef};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{Mutex, MutexGuard, mpsc};
use tracing::instrument::WithSubscriber as _;

use crate::connection::{ConnectionConfig, TlsMode};
use crate::driver::{
    Column, ColumnOrigin, Driver, DriverError, Flavor, Notice, NoticeSeverity, QueryEvent,
    QuerySummary, ServerInfo, TransactionState,
};
use crate::schema::{ColumnInfo, ForeignKeyInfo, IndexInfo, TableDetails, TableInfo, TableKind};
use crate::value::Value;

/// How many rows we send to the UI at a time.
const BATCH_SIZE: usize = 1000;

// The shape of each catalog query's result, in the order the columns are
// selected. sqlx fills these tuples straight from the rows. They are turned
// into the public `schema` types right after, so these names stay private.
/// name, type, nullable, default, identity flag, generated flag
type ColumnRow = (String, String, bool, Option<String>, String, String);
/// name, columns, unique, primary, method, definition
type IndexRow = (String, Vec<String>, bool, bool, String, String);
/// name, columns, referenced schema, referenced table, referenced columns,
/// on-update code, on-delete code
type ForeignKeyRow = (
    String,
    Vec<String>,
    String,
    String,
    Vec<String>,
    String,
    String,
);

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
    connect_timeout: Duration,
    keepalive: bool,
    /// The server's id for our main connection, as used by `pg_cancel_backend`.
    backend_pid: i32,
    /// True while a query is running, so `cancel` can skip when idle.
    busy: AtomicBool,
    /// A second connection used only for introspection (sidebar, Structure
    /// tab), so browsing the schema never waits for a long-running query.
    /// It is opened the first time it is needed, and quietly reopened if it
    /// has died (see `meta()`). `None` means "not open yet".
    meta: Mutex<Option<PgConnection>>,
    /// Names of custom types (enums and so on), by type number, looked up once
    /// and remembered. A plain std `Mutex` is fine here: it is only held for an
    /// instant, never across an `.await`.
    type_names: StdMutex<HashMap<u32, String>>,
    /// Tables seen in results (by table number), with their column names, so
    /// a result column can be traced back to `schema.table.column`.
    relations: StdMutex<HashMap<u32, Relation>>,
}

/// Exclusive use of the introspection connection (see `PostgresDriver::meta`).
struct Meta<'a> {
    guard: MutexGuard<'a, Option<PgConnection>>,
}

impl Meta<'_> {
    /// The connection itself. `meta()` always fills it in before returning, so
    /// the error is only a safety net, never expected.
    fn conn(&mut self) -> Result<&mut PgConnection, DriverError> {
        self.guard.as_mut().ok_or_else(|| {
            DriverError::Connection("the introspection connection is not open".into())
        })
    }
}

/// A table as the server knows it: its name and its columns by number.
struct Relation {
    schema: String,
    table: String,
    columns: HashMap<i16, String>,
}

impl PostgresDriver {
    /// Open a connection. `password` comes from the OS keychain (see `ConnectionConfig`).
    pub async fn connect(config: &ConnectionConfig, password: &str) -> Result<Self, DriverError> {
        let options = connect_options(config, password)?;
        let mut conn = open_connection(&options, config.connect_timeout).await?;
        if config.keepalive {
            conn = set_keepalive(conn).await;
        }
        // Remember this connection's id on the server. Cancel needs it later,
        // when the connection itself is too busy to answer.
        let backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut conn)
            .await
            .map_err(server_error)?;
        Ok(Self {
            conn: Mutex::new(conn),
            options,
            connect_timeout: config.connect_timeout,
            keepalive: config.keepalive,
            backend_pid,
            busy: AtomicBool::new(false),
            meta: Mutex::new(None),
            type_names: StdMutex::new(HashMap::new()),
            relations: StdMutex::new(HashMap::new()),
        })
    }

    /// The introspection connection. Opened on first use, and reopened if it
    /// has died (a server restart, a dropped network). Holding the returned
    /// guard means "I am using it"; others wait until it is dropped.
    ///
    /// Before handing it out we check it still answers. That costs one quick
    /// round trip per introspection call, which is cheap next to what the
    /// sidebar does with the result, and it means a dead connection can never
    /// break introspection for good.
    async fn meta(&self) -> Result<Meta<'_>, DriverError> {
        let mut guard = self.meta.lock().await;
        let alive = match guard.as_mut() {
            Some(conn) => conn.ping().await.is_ok(),
            None => false,
        };
        if !alive {
            let mut conn = open_connection(&self.options, self.connect_timeout).await?;
            if self.keepalive {
                conn = set_keepalive(conn).await;
            }
            *guard = Some(conn);
        }
        Ok(Meta { guard })
    }

    /// The column names, type names and origins of a result.
    ///
    /// sqlx can name the built-in types (INT4, TEXT, ...) but not custom ones
    /// (enums, domains, extension types): for those the server only sends a
    /// type number. The main connection is busy streaming, so we ask the
    /// introspection connection what the number means, once per type.
    async fn resolve_columns(&self, columns: &[PgColumn]) -> Vec<Column> {
        // Type numbers sqlx could not name, that we have not looked up yet.
        let missing: Vec<i64> = {
            let known = self
                .type_names
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            columns
                .iter()
                .filter(|c| c.type_info().name() == "?")
                .filter_map(|c| c.type_info().oid())
                .filter(|oid| !known.contains_key(&oid.0))
                .map(|oid| i64::from(oid.0))
                .collect()
        };
        if !missing.is_empty() {
            // Best effort: if the lookup fails the label stays "oid:12345".
            if let Ok(found) = self.lookup_type_names(&missing).await {
                let mut known = self
                    .type_names
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                known.extend(found);
            }
        }

        // The same for the tables the columns come from (table number 0 means
        // "not a table column", e.g. an expression).
        let missing_tables: Vec<i64> = {
            let known = self
                .relations
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let mut oids: Vec<i64> = columns
                .iter()
                .filter_map(|c| c.relation_id())
                .filter(|oid| oid.0 != 0 && !known.contains_key(&oid.0))
                .map(|oid| i64::from(oid.0))
                .collect();
            oids.sort_unstable();
            oids.dedup();
            oids
        };
        if !missing_tables.is_empty()
            && let Ok(found) = self.lookup_relations(&missing_tables).await
        {
            self.relations
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(found);
        }
        let relations = self
            .relations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        let known = self
            .type_names
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        columns
            .iter()
            .map(|c| {
                let info = c.type_info();
                let type_name = if info.name() != "?" {
                    info.name().to_string()
                } else {
                    match info.oid() {
                        Some(oid) => known
                            .get(&oid.0)
                            .cloned()
                            .unwrap_or_else(|| format!("oid:{}", oid.0)),
                        None => "unknown".to_string(),
                    }
                };
                let origin =
                    c.relation_id()
                        .zip(c.relation_attribute_no())
                        .and_then(|(table, number)| {
                            let relation = relations.get(&table.0)?;
                            Some(ColumnOrigin {
                                schema: relation.schema.clone(),
                                table: relation.table.clone(),
                                column: relation.columns.get(&number)?.clone(),
                            })
                        });
                Column {
                    name: c.name().to_string(),
                    type_name,
                    origin,
                }
            })
            .collect()
    }

    /// Ask the server which tables these table numbers are, and what their
    /// columns are called. Dropped columns are skipped.
    async fn lookup_relations(&self, oids: &[i64]) -> Result<Vec<(u32, Relation)>, DriverError> {
        let mut meta = self.meta().await?;
        let conn = meta.conn()?;
        let rows: Vec<(i64, String, String, i32, String)> = sqlx::query_as(
            "SELECT c.oid::int8, n.nspname::text, c.relname::text, \
                    a.attnum::int4, a.attname::text \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped \
             WHERE c.oid::int8 = ANY($1)",
        )
        .bind(oids.to_vec())
        .fetch_all(&mut *conn)
        .await
        .map_err(server_error)?;

        let mut found: HashMap<u32, Relation> = HashMap::new();
        for (oid, schema, table, number, column) in rows {
            let (Ok(oid), Ok(number)) = (u32::try_from(oid), i16::try_from(number)) else {
                continue;
            };
            found
                .entry(oid)
                .or_insert_with(|| Relation {
                    schema,
                    table,
                    columns: HashMap::new(),
                })
                .columns
                .insert(number, column);
        }
        Ok(found.into_iter().collect())
    }

    /// Ask the server what these type numbers are called.
    /// An array type is shown as its element type plus `[]` (e.g. `mood[]`),
    /// the same way sqlx names built-in arrays (`INT4[]`).
    async fn lookup_type_names(&self, oids: &[i64]) -> Result<Vec<(u32, String)>, DriverError> {
        let mut meta = self.meta().await?;
        let conn = meta.conn()?;
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT t.oid::int8, \
                    CASE WHEN t.typcategory = 'A' THEN e.typname::text || '[]' \
                         ELSE t.typname::text END \
             FROM pg_type t LEFT JOIN pg_type e ON e.oid = t.typelem \
             WHERE t.oid::int8 = ANY($1)",
        )
        .bind(oids.to_vec())
        .fetch_all(&mut *conn)
        .await
        .map_err(server_error)?;
        Ok(rows
            .into_iter()
            .filter_map(|(oid, name)| u32::try_from(oid).ok().map(|oid| (oid, name)))
            .collect())
    }

    /// The real work of `run_query`, split out so the wrapper can reset `busy`
    /// no matter how this function ends.
    async fn stream_query(
        &self,
        sql: &str,
        tx: &mpsc::Sender<QueryEvent>,
        mut notices: UnboundedReceiver<Notice>,
    ) -> Result<QuerySummary, DriverError> {
        let mut conn = self.conn.lock().await;
        self.busy.store(true, Ordering::SeqCst);
        // `fetch_many` yields two kinds of item, in order: rows (`Right`) and,
        // after each statement finishes, that statement's summary (`Left`).
        // The SQL may hold several statements, so there can be several result sets.
        let mut stream = sqlx::raw_sql(sql).fetch_many(&mut *conn);

        let outcome = pump(self, &mut stream, tx, &mut notices).await;
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
        // A statement that returned no rows never told us its columns (the
        // server only describes them alongside the first row). If the whole
        // run returned nothing and changed nothing, it may have been a SELECT
        // with an empty result, so ask for its column list.
        if let Ok(summary) = &outcome
            && summary.rows_returned == 0
            && summary.rows_affected == 0
        {
            let columns = self.describe_empty_result(sql).await;
            if !columns.is_empty() {
                send(tx, QueryEvent::Columns(columns)).await?;
            }
        }
        outcome
    }

    /// The column list of a statement that returned no rows, so the UI can
    /// still show the headers. Best effort: any problem gives an empty list.
    ///
    /// This asks the server to *describe* the statement (parse it without
    /// running it) on the separate introspection connection, never the main
    /// one. A failed describe makes Postgres mark the current transaction as
    /// aborted, and the main connection may be inside the user's own open
    /// transaction; the introspection connection never is, so a failure there
    /// cannot hurt anyone.
    ///
    /// What this means in practice:
    /// - several statements in one run: the describe fails, so no headers;
    /// - a statement with no result columns (INSERT, DDL, SET): no headers, as
    ///   there is nothing to show;
    /// - tables only the main session can see (temp tables, or tables created
    ///   in the user's still-open transaction) cannot be described: no headers;
    /// - the introspection connection's own `search_path` is used to find
    ///   tables, so if the user changed theirs, an unqualified name could in
    ///   rare cases resolve to a different table with the same name.
    async fn describe_empty_result(&self, sql: &str) -> Vec<Column> {
        let described = {
            let Ok(mut meta) = self.meta().await else {
                return Vec::new();
            };
            let Ok(conn) = meta.conn() else {
                return Vec::new();
            };
            match (&mut *conn).describe(sql).await {
                Ok(described) => described,
                Err(_) => return Vec::new(),
            }
            // `conn` is released here, before resolve_columns uses it again.
        };
        self.resolve_columns(described.columns()).await
    }
}

/// Read rows from the server and forward them to the UI in batches.
///
/// `S` can be any stream of sqlx results; `Unpin` just means it may be
/// polled by reference (`&mut`), which `stream.next()` needs.
async fn pump<S>(
    driver: &PostgresDriver,
    stream: &mut S,
    tx: &mpsc::Sender<QueryEvent>,
    notices: &mut UnboundedReceiver<Notice>,
) -> Result<QuerySummary, DriverError>
where
    S: Stream<Item = Result<Either<PgQueryResult, PgRow>, sqlx::Error>> + Unpin,
{
    let mut summary = QuerySummary::default();
    let mut batch: Vec<Vec<Value>> = Vec::with_capacity(BATCH_SIZE);
    let mut columns_sent = false;
    // Type names of the current result, computed once from its first row and
    // reused for every row after it.
    let mut type_names: Vec<String> = Vec::new();
    let mut rows_in_statement: u64 = 0;

    while let Some(item) = stream.next().await {
        // Notices the server sent while we waited, in the order it sent them.
        forward_notices(notices, &mut batch, tx).await?;
        let item = match item {
            Ok(item) => item,
            Err(error) => {
                // The statement failed partway. Rows already read are still
                // good data the user should see, so send them before the error.
                if !batch.is_empty() {
                    send(tx, QueryEvent::Rows(std::mem::take(&mut batch))).await?;
                }
                return Err(server_error(error));
            }
        };
        match item {
            Either::Right(row) => {
                if !columns_sent {
                    let columns = driver.resolve_columns(row.columns()).await;
                    type_names = columns.iter().map(|c| c.type_name.clone()).collect();
                    send(tx, QueryEvent::Columns(columns)).await?;
                    columns_sent = true;
                }
                batch.push(convert_row(&row, &type_names));
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
    forward_notices(notices, &mut batch, tx).await?;
    Ok(summary)
}

/// Pass on any notices that have arrived. Rows already read go out first, so
/// the UI sees things in the order the server did them.
async fn forward_notices(
    notices: &mut UnboundedReceiver<Notice>,
    batch: &mut Vec<Vec<Value>>,
    tx: &mpsc::Sender<QueryEvent>,
) -> Result<(), DriverError> {
    while let Ok(notice) = notices.try_recv() {
        if !batch.is_empty() {
            send(tx, QueryEvent::Rows(std::mem::take(batch))).await?;
        }
        send(tx, QueryEvent::Notice(notice)).await?;
    }
    Ok(())
}

/// A tiny `tracing` listener that picks out the server's notices.
///
/// sqlx logs every notice as an event whose target is
/// "sqlx::postgres::notice" and whose level encodes the severity. We ignore
/// everything else and forward those through a channel.
struct NoticeCollector {
    tx: mpsc::UnboundedSender<Notice>,
}

impl tracing::Subscriber for NoticeCollector {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.target() == NOTICE_TARGET
    }

    fn event(&self, event: &tracing::Event<'_>) {
        if event.metadata().target() != NOTICE_TARGET {
            return;
        }
        let level = *event.metadata().level();
        let severity = if level == tracing::Level::ERROR {
            NoticeSeverity::Error
        } else if level == tracing::Level::WARN {
            NoticeSeverity::Warning
        } else if level == tracing::Level::INFO {
            NoticeSeverity::Notice
        } else if level == tracing::Level::DEBUG {
            NoticeSeverity::Debug
        } else {
            NoticeSeverity::Info
        };
        let mut message = MessageText::default();
        event.record(&mut message);
        // If the receiver is gone the query is over; nothing to do.
        let _ = self.tx.send(Notice {
            severity,
            message: message.text,
        });
    }

    // We never use spans (timed sections), but the trait requires these.
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// The `target` sqlx puts on its notice events.
const NOTICE_TARGET: &str = "sqlx::postgres::notice";

/// Reads the `message` field out of a `tracing` event.
#[derive(Default)]
struct MessageText {
    text: String,
}

impl tracing::field::Visit for MessageText {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.text = value.to_string();
        }
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" && self.text.is_empty() {
            self.text = format!("{value:?}");
        }
    }
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
        // sqlx reports the server's notices only as `tracing` log events. Run
        // the query with a listener attached to just this query's future, so
        // the notices it triggers can be passed on to the caller, and no one
        // else's. (Other queries running at the same time are not affected.)
        let (notice_tx, notice_rx) = mpsc::unbounded_channel();
        let listener = tracing::Dispatch::new(NoticeCollector { tx: notice_tx });
        let result = self
            .stream_query(sql, &tx, notice_rx)
            .with_subscriber(listener)
            .await;
        self.busy.store(false, Ordering::SeqCst);
        result
    }

    async fn transaction_state(&self) -> Result<TransactionState, DriverError> {
        // If a query holds the connection, we cannot ask. Do not wait for it.
        let Ok(mut conn) = self.conn.try_lock() else {
            return Ok(TransactionState::Busy);
        };
        // `transaction_timestamp()` is when the current transaction began and
        // `statement_timestamp()` is when this statement began. With no open
        // transaction they are the same moment; inside one (after BEGIN) the
        // transaction began earlier. We use the simple query protocol so both
        // come from a single client message.
        let mut stream =
            sqlx::raw_sql("SELECT (transaction_timestamp() = statement_timestamp())::text")
                .fetch_many(&mut *conn);
        let mut same: Option<String> = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(Either::Right(row)) => same = row.try_get(0).ok(),
                Ok(Either::Left(_)) => {}
                // 25P02 = "current transaction is aborted": a statement failed
                // inside a transaction and the server now refuses everything.
                Err(error) => {
                    return match server_error(error) {
                        DriverError::Server { code, .. } if code.as_deref() == Some("25P02") => {
                            Ok(TransactionState::Aborted)
                        }
                        other => Err(other),
                    };
                }
            }
        }
        match same.as_deref() {
            Some("true") => Ok(TransactionState::Idle),
            Some(_) => Ok(TransactionState::Open),
            // The server answered without a row: not something to guess about.
            None => Err(DriverError::Connection(
                "the server gave no answer to the transaction check".to_string(),
            )),
        }
    }

    async fn cancel(&self) -> Result<(), DriverError> {
        if !self.busy.load(Ordering::SeqCst) {
            // Nothing is running. Sending a cancel now could hit the *next*
            // query instead (Postgres cancels "whatever runs right now").
            return Ok(());
        }
        // The main connection is busy, so open a second one just for this.
        let mut other = open_connection(&self.options, self.connect_timeout).await?;
        sqlx::query("SELECT pg_cancel_backend($1)")
            .bind(self.backend_pid)
            .execute(&mut other)
            .await
            .map_err(server_error)?;
        // Closing politely is optional; the server notices when we drop it.
        let _ = other.close().await;
        Ok(())
    }

    async fn list_databases(&self) -> Result<Vec<String>, DriverError> {
        let mut meta = self.meta().await?;
        let conn = meta.conn()?;
        // Skip the template databases and any that refuse connections.
        sqlx::query_scalar(
            "SELECT datname::text FROM pg_database \
             WHERE NOT datistemplate AND datallowconn ORDER BY datname",
        )
        .fetch_all(&mut *conn)
        .await
        .map_err(server_error)
    }

    async fn list_schemas(&self) -> Result<Vec<String>, DriverError> {
        let mut meta = self.meta().await?;
        let conn = meta.conn()?;
        // Hide Postgres's internal schemas: information_schema, pg_catalog,
        // pg_toast and the per-session pg_temp_N ones (all start with "pg_").
        sqlx::query_scalar(
            "SELECT nspname::text FROM pg_namespace \
             WHERE nspname <> 'information_schema' AND nspname NOT LIKE 'pg\\_%' \
             ORDER BY nspname",
        )
        .fetch_all(&mut *conn)
        .await
        .map_err(server_error)
    }

    async fn list_tables(&self, schema: &str) -> Result<Vec<TableInfo>, DriverError> {
        let mut meta = self.meta().await?;
        let conn = meta.conn()?;
        // pg_class holds tables, views, indexes and sequences alike; `relkind`
        // says which. We keep only the kinds a person browses:
        //   r = table, p = partitioned table, v = view,
        //   m = materialized view, f = foreign table.
        // `reltuples` is the planner's row estimate (-1 = never analyzed).
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT c.relname::text, c.relkind::text, c.reltuples::bigint \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f') \
             ORDER BY c.relname",
        )
        .bind(schema)
        .fetch_all(&mut *conn)
        .await
        .map_err(server_error)?;

        Ok(rows
            .into_iter()
            .map(|(name, relkind, reltuples)| TableInfo {
                name,
                kind: match relkind.as_str() {
                    "v" => TableKind::View,
                    "m" => TableKind::MaterializedView,
                    "f" => TableKind::ForeignTable,
                    _ => TableKind::Table,
                },
                estimated_rows: (reltuples >= 0).then_some(reltuples),
            })
            .collect())
    }

    async fn describe_table(&self, schema: &str, table: &str) -> Result<TableDetails, DriverError> {
        let mut meta = self.meta().await?;
        let conn = meta.conn()?;

        // `format('%I.%I', ...)::regclass` turns "schema" + "table" into the
        // server's internal id for that table. `%I` quotes names that need it
        // (capitals, spaces). If the table does not exist, the server itself
        // answers 'relation "..." does not exist', and we pass that through.
        const TABLE_ID: &str = "format('%I.%I', $1::text, $2::text)::regclass";

        // Columns. `attnum > 0` skips hidden system columns and
        // `NOT attisdropped` skips columns that were dropped earlier.
        // `format_type` gives the readable type, e.g. "numeric(12,2)".
        // `attidentity` is '' for normal columns, 'a'/'d' for identity ones.
        // `attgenerated` is '' unless the column is computed by the server.
        let columns: Vec<ColumnRow> = sqlx::query_as(&format!(
            "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), \
                        NOT a.attnotnull, pg_get_expr(d.adbin, d.adrelid), \
                        a.attidentity::text, a.attgenerated::text \
                 FROM pg_attribute a \
                 LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE a.attrelid = {TABLE_ID} AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum"
        ))
        .bind(schema)
        .bind(table)
        .fetch_all(&mut *conn)
        .await
        .map_err(server_error)?;

        // Indexes (the primary key is just an index with indisprimary = true).
        // `indkey` lists the column numbers; we look their names up in order.
        // Number 0 means an expression, which has no plain column name.
        let indexes: Vec<IndexRow> =
            sqlx::query_as(&format!(
                "SELECT i.relname::text, \
                        COALESCE((SELECT array_agg(a.attname::text ORDER BY k.ord) \
                                  FROM unnest(ix.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
                                  JOIN pg_attribute a ON a.attrelid = ix.indrelid AND a.attnum = k.attnum \
                                  WHERE k.attnum > 0), ARRAY[]::text[]), \
                        ix.indisunique, ix.indisprimary, am.amname::text, \
                        pg_get_indexdef(ix.indexrelid) \
                 FROM pg_index ix \
                 JOIN pg_class i ON i.oid = ix.indexrelid \
                 JOIN pg_am am ON am.oid = i.relam \
                 WHERE ix.indrelid = {TABLE_ID} \
                 ORDER BY ix.indisprimary DESC, i.relname"
            ))
            .bind(schema)
            .bind(table)
            .fetch_all(&mut *conn)
            .await
            .map_err(server_error)?;

        // Foreign keys: `conkey` are this table's column numbers and `confkey`
        // the referenced table's, matched up by position. The one-letter
        // action codes are translated in Rust below.
        let foreign_keys: Vec<ForeignKeyRow> =
            sqlx::query_as(&format!(
                "SELECT con.conname::text, \
                        COALESCE((SELECT array_agg(a.attname::text ORDER BY k.ord) \
                                  FROM unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord) \
                                  JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum), \
                                 ARRAY[]::text[]), \
                        rn.nspname::text, rc.relname::text, \
                        COALESCE((SELECT array_agg(a.attname::text ORDER BY k.ord) \
                                  FROM unnest(con.confkey) WITH ORDINALITY AS k(attnum, ord) \
                                  JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.attnum), \
                                 ARRAY[]::text[]), \
                        con.confupdtype::text, con.confdeltype::text \
                 FROM pg_constraint con \
                 JOIN pg_class rc ON rc.oid = con.confrelid \
                 JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
                 WHERE con.contype = 'f' AND con.conrelid = {TABLE_ID} \
                 ORDER BY con.conname"
            ))
            .bind(schema)
            .bind(table)
            .fetch_all(&mut *conn)
            .await
            .map_err(server_error)?;

        let indexes: Vec<IndexInfo> = indexes
            .into_iter()
            .map(
                |(name, columns, unique, primary, method, definition)| IndexInfo {
                    name,
                    columns,
                    unique,
                    primary,
                    method,
                    definition,
                },
            )
            .collect();
        // The primary key is the columns of the index marked primary.
        let primary_key = indexes
            .iter()
            .find(|index| index.primary)
            .map(|index| index.columns.clone())
            .unwrap_or_default();

        Ok(TableDetails {
            columns: columns
                .into_iter()
                .map(
                    |(name, data_type, nullable, default, identity, generated)| ColumnInfo {
                        name,
                        data_type,
                        nullable,
                        default,
                        identity: !identity.is_empty(),
                        generated: !generated.is_empty(),
                    },
                )
                .collect(),
            primary_key,
            indexes,
            foreign_keys: foreign_keys
                .into_iter()
                .map(
                    |(name, columns, ref_schema, ref_table, ref_columns, on_update, on_delete)| {
                        ForeignKeyInfo {
                            name,
                            columns,
                            referenced_schema: ref_schema,
                            referenced_table: ref_table,
                            referenced_columns: ref_columns,
                            on_update: fk_action(&on_update),
                            on_delete: fk_action(&on_delete),
                        }
                    },
                )
                .collect(),
        })
    }
}

/// Postgres stores foreign key actions as one letter; show the SQL words.
fn fk_action(code: &str) -> String {
    match code {
        "a" => "NO ACTION",
        "r" => "RESTRICT",
        "c" => "CASCADE",
        "n" => "SET NULL",
        "d" => "SET DEFAULT",
        // A code from a future Postgres version: show it rather than hide it.
        other => other,
    }
    .to_string()
}

/// Translate our connection settings into sqlx's.
fn connect_options(
    config: &ConnectionConfig,
    password: &str,
) -> Result<PgConnectOptions, DriverError> {
    let tls = &config.tls;
    let mut options = PgConnectOptions::new()
        .host(&config.host)
        .port(config.port)
        .username(&config.user)
        .password(password)
        .database(&config.database)
        .application_name(&config.application_name)
        .ssl_mode(match tls.mode {
            TlsMode::Disable => PgSslMode::Disable,
            TlsMode::Prefer => PgSslMode::Prefer,
            TlsMode::Require => PgSslMode::Require,
            TlsMode::VerifyCa => PgSslMode::VerifyCa,
            TlsMode::VerifyFull => PgSslMode::VerifyFull,
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
        options = options.ssl_root_cert(path);
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
    options: &PgConnectOptions,
    timeout: Duration,
) -> Result<PgConnection, DriverError> {
    match tokio::time::timeout(timeout, PgConnection::connect_with(options)).await {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(error)) => Err(DriverError::Connection(error.to_string())),
        Err(_) => Err(DriverError::Connection(format!(
            "no answer from the server after {} seconds",
            timeout.as_secs()
        ))),
    }
}

/// Ask the server to send TCP keepalive probes on this connection: after 60
/// idle seconds, then every 10 seconds, giving up after 3 misses. That keeps
/// the connection alive through routers and load balancers that drop quiet
/// connections, and lets the server clean up if we vanish.
///
/// Best effort: a server or proxy that refuses these settings is fine, we just
/// go without. (They are only a request to the server, not a Rowlark feature.)
///
/// The connection is passed in and handed back (not borrowed) because the
/// compiler cannot prove sqlx's borrowed form is `Send` inside a helper like
/// this; owning it for the whole call avoids the problem.
async fn set_keepalive(mut conn: PgConnection) -> PgConnection {
    // `set_config(name, value, false)` is the function form of `SET name = value`
    // (false = keep it for the whole session, not just this transaction).
    let _ = sqlx::query(
        "SELECT set_config('tcp_keepalives_idle', '60', false), \
                set_config('tcp_keepalives_interval', '10', false), \
                set_config('tcp_keepalives_count', '3', false)",
    )
    .execute(&mut conn)
    .await;
    conn
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

/// Convert one row. `type_names` holds each column's type name (same order).
fn convert_row(row: &PgRow, type_names: &[String]) -> Vec<Value> {
    type_names
        .iter()
        .enumerate()
        .map(|(i, type_name)| convert_cell(row, i, type_name))
        .collect()
}

/// Read one cell as the server's text and wrap it in the right `Value`.
fn convert_cell(row: &PgRow, index: usize, type_name: &str) -> Value {
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
    // We ask the server for UTF-8, so text normally is. The exception is a
    // database created as SQL_ASCII, which stores bytes without knowing what
    // they mean. Rather than corrupt such text with replacement characters,
    // hand over the exact bytes.
    match std::str::from_utf8(bytes) {
        Ok(text) => text_to_value(type_name, text),
        Err(_) => Value::Bytes(bytes.to_vec()),
    }
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
    // Arrays are named "<element type>[]", e.g. "INT4[]".
    if let Some(element_type) = type_name.strip_suffix("[]") {
        return array_to_value(element_type, text).unwrap_or_else(unknown);
    }
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

/// An array after parsing its text, before its elements become `Value`s.
enum ArrayNode {
    Null,
    Item(String),
    List(Vec<ArrayNode>),
}

/// Turn Postgres array text like `{1,NULL,"a b"}` into `Value::Array`, converting
/// each element by its element type. Returns `None` for text we cannot read
/// safely, so the caller can fall back to `Unknown` with the original text.
fn array_to_value(element_type: &str, text: &str) -> Option<Value> {
    // The `box` type separates its elements with ';' instead of ',', which
    // this reader does not handle. Show those as plain text instead.
    if element_type.eq_ignore_ascii_case("box") {
        return None;
    }
    let chars: Vec<char> = text.chars().collect();
    let mut pos = 0;
    let node = parse_list(&chars, &mut pos)?;
    // Everything must be used up; leftovers mean we misread something.
    if pos != chars.len() {
        return None;
    }
    Some(node_to_value(element_type, node))
}

fn node_to_value(element_type: &str, node: ArrayNode) -> Value {
    match node {
        ArrayNode::Null => Value::Null,
        ArrayNode::Item(text) => text_to_value(element_type, &text),
        ArrayNode::List(items) => Value::Array(
            items
                .into_iter()
                .map(|item| node_to_value(element_type, item))
                .collect(),
        ),
    }
}

/// Read `{ ... }` starting at `pos`. A leading `[1:2]=` (an array that does not
/// start at index 1) is not understood and gives `None`.
fn parse_list(chars: &[char], pos: &mut usize) -> Option<ArrayNode> {
    if chars.get(*pos)? != &'{' {
        return None;
    }
    *pos += 1;
    let mut items = Vec::new();
    if chars.get(*pos) == Some(&'}') {
        *pos += 1;
        return Some(ArrayNode::List(items));
    }
    loop {
        items.push(parse_element(chars, pos)?);
        match chars.get(*pos)? {
            ',' => *pos += 1,
            '}' => {
                *pos += 1;
                return Some(ArrayNode::List(items));
            }
            _ => return None,
        }
    }
}

/// Read one element: a nested list, a "quoted string", or a bare word.
fn parse_element(chars: &[char], pos: &mut usize) -> Option<ArrayNode> {
    match chars.get(*pos)? {
        '{' => parse_list(chars, pos),
        '"' => {
            *pos += 1;
            let mut text = String::new();
            loop {
                match chars.get(*pos)? {
                    '"' => {
                        *pos += 1;
                        return Some(ArrayNode::Item(text));
                    }
                    // A backslash makes the next character literal (\" or \\).
                    '\\' => {
                        *pos += 1;
                        text.push(*chars.get(*pos)?);
                        *pos += 1;
                    }
                    other => {
                        text.push(*other);
                        *pos += 1;
                    }
                }
            }
        }
        _ => {
            let start = *pos;
            while let Some(c) = chars.get(*pos) {
                if *c == ',' || *c == '}' {
                    break;
                }
                *pos += 1;
            }
            let word: String = chars[start..*pos].iter().collect();
            // An unquoted NULL is a real NULL; a quoted "NULL" is the text.
            if word.eq_ignore_ascii_case("NULL") {
                Some(ArrayNode::Null)
            } else {
                Some(ArrayNode::Item(word))
            }
        }
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
    use crate::connection::TlsConfig;

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

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    #[test]
    fn every_other_type_maps_to_its_own_variant() {
        let cases: [(&str, &str, Value); 14] = [
            ("INT2", "-32768", Value::Int(-32768)),
            ("FLOAT4", "1.5", Value::Float(1.5)),
            ("OID", "4294967295", Value::UInt(4_294_967_295)),
            ("VARCHAR", "abc", text("abc")),
            ("BPCHAR", "ab   ", text("ab   ")),
            ("NAME", "pg_catalog", text("pg_catalog")),
            ("CHAR", "x", text("x")),
            ("JSON", "[1, 2]", Value::Json("[1, 2]".into())),
            ("DATE", "2026-11-24", Value::Date("2026-11-24".into())),
            ("TIME", "13:45:10", Value::Time("13:45:10".into())),
            (
                "TIMETZ",
                "13:45:10+05:30",
                Value::Time("13:45:10+05:30".into()),
            ),
            (
                "TIMESTAMP",
                "2026-11-24 13:45:10",
                Value::Timestamp {
                    iso: "2026-11-24 13:45:10".into(),
                    tz: None,
                },
            ),
            (
                "UUID",
                "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
                Value::Uuid("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into()),
            ),
            ("BOOL", "t", Value::Bool(true)),
        ];
        for (type_name, input, expected) in cases {
            assert_eq!(text_to_value(type_name, input), expected, "{type_name}");
        }
        // Values that do not fit their type fall back instead of failing.
        for (type_name, input) in [
            ("BOOL", "maybe"),
            ("OID", "-1"),
            ("FLOAT8", "x"),
            ("BYTEA", "\\xzz"),
            ("BYTEA", "\\xabc"),
        ] {
            assert!(
                matches!(text_to_value(type_name, input), Value::Unknown { .. }),
                "{type_name} {input}"
            );
        }
    }

    #[test]
    fn foreign_key_action_codes_become_sql_words() {
        for (code, words) in [
            ("a", "NO ACTION"),
            ("r", "RESTRICT"),
            ("c", "CASCADE"),
            ("n", "SET NULL"),
            ("d", "SET DEFAULT"),
            ("z", "z"), // a future code is shown, not hidden
        ] {
            assert_eq!(fk_action(code), words);
        }
    }

    fn config_with(tls: TlsConfig) -> ConnectionConfig {
        let mut config = ConnectionConfig::new("db.example.com", 6543, "alice", "shop");
        config.tls = tls;
        config
    }

    #[test]
    fn connection_settings_reach_sqlx() {
        let mut config = config_with(TlsConfig::new(TlsMode::Disable));
        config.application_name = "Report".into();
        let options = connect_options(&config, "pw").expect("options");
        assert_eq!(options.get_host(), "db.example.com");
        assert_eq!(options.get_port(), 6543);
        assert_eq!(options.get_username(), "alice");
        assert_eq!(options.get_database(), Some("shop"));
        assert_eq!(options.get_application_name(), Some("Report"));
    }

    #[test]
    fn every_tls_mode_maps_to_the_matching_sqlx_mode() {
        for (mode, expected) in [
            (TlsMode::Disable, "Disable"),
            (TlsMode::Prefer, "Prefer"),
            (TlsMode::Require, "Require"),
            (TlsMode::VerifyCa, "VerifyCa"),
            (TlsMode::VerifyFull, "VerifyFull"),
        ] {
            let options =
                connect_options(&config_with(TlsConfig::new(mode)), "pw").expect("options");
            assert_eq!(format!("{:?}", options.get_ssl_mode()), expected);
        }
    }

    #[test]
    fn a_missing_certificate_file_is_named_in_the_error() {
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
            match connect_options(&config_with(tls), "pw") {
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
    fn error_messages_read_well_for_people() {
        assert_eq!(DriverError::Cancelled.to_string(), "query cancelled");
        // A server error is the server's own sentence, with nothing added.
        let server = DriverError::Server {
            code: Some("42501".into()),
            message: "permission denied for table t".into(),
        };
        assert_eq!(server.to_string(), "permission denied for table t");
        assert!(
            DriverError::Connection("refused".into())
                .to_string()
                .contains("refused")
        );
        assert!(
            DriverError::InvalidConfig("bad".into())
                .to_string()
                .contains("bad")
        );
        assert!(!DriverError::ReceiverClosed.to_string().is_empty());
    }

    #[test]
    fn arrays_of_numbers_parse() {
        assert_eq!(
            text_to_value("INT4[]", "{1,2,3}"),
            Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
        );
        assert_eq!(text_to_value("INT4[]", "{}"), Value::Array(vec![]));
    }

    #[test]
    fn array_nulls_are_real_but_quoted_null_is_text() {
        assert_eq!(
            text_to_value("TEXT[]", r#"{NULL,"NULL",null}"#),
            Value::Array(vec![Value::Null, text("NULL"), Value::Null])
        );
    }

    #[test]
    fn array_quoting_and_backslashes_are_undone() {
        assert_eq!(
            text_to_value(
                "TEXT[]",
                r#"{"a b","c,d","say \"hi\"","back\\slash","","😀"}"#
            ),
            Value::Array(vec![
                text("a b"),
                text("c,d"),
                text("say \"hi\""),
                text("back\\slash"),
                text(""),
                text("😀"),
            ])
        );
    }

    #[test]
    fn nested_arrays_keep_their_shape() {
        assert_eq!(
            text_to_value("INT4[]", "{{1,2},{3,NULL}}"),
            Value::Array(vec![
                Value::Array(vec![Value::Int(1), Value::Int(2)]),
                Value::Array(vec![Value::Int(3), Value::Null]),
            ])
        );
    }

    #[test]
    fn array_elements_keep_their_exact_text() {
        assert_eq!(
            text_to_value("NUMERIC[]", "{0.10,NaN}"),
            Value::Array(vec![
                Value::Decimal("0.10".into()),
                Value::Decimal("NaN".into())
            ])
        );
        // An element type we do not know keeps the server's text and type name.
        assert_eq!(
            text_to_value("mood[]", "{sad,happy}"),
            Value::Array(vec![
                Value::Unknown {
                    type_name: "mood".into(),
                    text: "sad".into()
                },
                Value::Unknown {
                    type_name: "mood".into(),
                    text: "happy".into()
                },
            ])
        );
    }

    #[test]
    fn arrays_we_cannot_read_safely_stay_as_text() {
        for bad in [
            "[0:1]={1,2}",
            "{1,2",
            "{1,,",
            "1,2",
            "{1}x",
            "{\"open",
            "{\"a\"b}", // junk right after a quoted element
        ] {
            assert!(
                matches!(text_to_value("INT4[]", bad), Value::Unknown { .. }),
                "{bad}"
            );
        }
        // box uses ';' between elements, which we do not read.
        assert!(matches!(
            text_to_value("BOX[]", "{(1,2),(3,4)}"),
            Value::Unknown { .. }
        ));
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
