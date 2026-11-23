//! Integration tests for the Postgres driver, run against the Docker servers.
//!
//! These need `docker compose up -d` first, so they are `#[ignore]`d and a
//! plain `cargo test` skips them (CI has no Docker). Run them with:
//!
//!     cargo test -p rowlark-core --test postgres -- --ignored
//!
//! Ports come from docker-compose.yml: Postgres 14..17 -> 54314..54317.

use rowlark_core::{
    Column, ConnectionConfig, Driver, DriverError, Flavor, PostgresDriver, QueryEvent,
    QuerySummary, Value,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

fn config(port: u16) -> ConnectionConfig {
    ConnectionConfig::new("127.0.0.1", port, "rowlark", "rowlark_test")
}

/// Connect to the Postgres 16 test server.
async fn pg16() -> PostgresDriver {
    PostgresDriver::connect(&config(54316), "rowlark")
        .await
        .expect("is `docker compose up -d` running?")
}

/// Everything one `run_query` call produced.
struct Run {
    summary: Result<QuerySummary, DriverError>,
    events: Vec<QueryEvent>,
}

impl Run {
    /// All rows from all result sets, in order.
    fn rows(&self) -> Vec<&Vec<Value>> {
        self.events
            .iter()
            .filter_map(|e| match e {
                QueryEvent::Rows(rows) => Some(rows.iter()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn columns(&self) -> Vec<&Vec<Column>> {
        self.events
            .iter()
            .filter_map(|e| match e {
                QueryEvent::Columns(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    /// Sizes of each `Rows` batch, to check batching.
    fn batch_sizes(&self) -> Vec<usize> {
        self.events
            .iter()
            .filter_map(|e| match e {
                QueryEvent::Rows(rows) => Some(rows.len()),
                _ => None,
            })
            .collect()
    }
}

/// Run a query and collect everything it sends.
///
/// The query and the collector must run at the same time: the channel holds
/// only a few messages, so a query that sends a lot would wait for us to read.
/// `tokio::join!` runs both side by side.
async fn run(driver: &PostgresDriver, sql: &str) -> Run {
    let (tx, mut rx) = mpsc::channel(4);
    let collect = async {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    };
    // `run_query` takes `tx` by value, so the channel closes when it finishes
    // and the collector's loop ends.
    let (summary, events) = tokio::join!(driver.run_query(sql, tx), collect);
    Run { summary, events }
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn ping_works_on_every_postgres_version() {
    for (port, prefix) in [
        (54314, "PostgreSQL 14"),
        (54315, "PostgreSQL 15"),
        (54316, "PostgreSQL 16"),
        (54317, "PostgreSQL 17"),
    ] {
        let driver = PostgresDriver::connect(&config(port), "rowlark")
            .await
            .unwrap_or_else(|e| panic!("port {port}: {e}"));
        let info = driver.ping().await.expect("ping");
        assert_eq!(info.flavor, Flavor::Postgres);
        assert!(
            info.version.starts_with(prefix),
            "port {port}: got {}",
            info.version
        );
    }
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn wrong_password_is_a_connection_error() {
    let result = PostgresDriver::connect(&config(54316), "wrong-password").await;
    assert!(matches!(result, Err(DriverError::Connection(_))));
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn simple_select_returns_columns_and_rows() {
    let driver = pg16().await;
    let r = run(&driver, "SELECT 1 AS n, 'hi' AS greeting").await;

    let summary = r.summary.as_ref().expect("query");
    assert_eq!(summary.rows_returned, 1);
    assert_eq!(summary.rows_affected, 0);

    let columns = r.columns();
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0][0].name, "n");
    assert_eq!(columns[0][0].type_name, "INT4");
    assert_eq!(columns[0][1].name, "greeting");

    assert_eq!(
        r.rows(),
        vec![&vec![Value::Int(1), Value::Text("hi".into())]]
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn big_results_arrive_in_batches_of_1000() {
    let driver = pg16().await;
    let r = run(&driver, "SELECT g FROM generate_series(1, 2500) AS g").await;
    assert_eq!(r.summary.as_ref().expect("query").rows_returned, 2500);
    assert_eq!(r.batch_sizes(), vec![1000, 1000, 500]);
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn values_keep_the_servers_exact_text() {
    let driver = pg16().await;
    let r = run(
        &driver,
        "SELECT 0.10::numeric AS d, \
                9223372036854775807::bigint AS big, \
                '{\"b\": 1,   \"a\": 1.10}'::json AS j, \
                NULL::int AS nothing, \
                'a😀b'::text AS emoji",
    )
    .await;
    r.summary.as_ref().expect("query");
    assert_eq!(
        r.rows(),
        vec![&vec![
            Value::Decimal("0.10".into()),
            Value::Int(i64::MAX),
            // `json` (unlike `jsonb`) keeps spacing and key order exactly.
            Value::Json("{\"b\": 1,   \"a\": 1.10}".into()),
            Value::Null,
            Value::Text("a😀b".into()),
        ]]
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn timestamps_keep_the_session_offset() {
    let driver = pg16().await;
    run(&driver, "SET TIME ZONE 'Europe/Berlin'")
        .await
        .summary
        .expect("set time zone");
    // 02:30 at +01:00 is 01:30 UTC, which is after Berlin's 2026-03-29 clock change.
    let r = run(&driver, "SELECT '2026-03-29 02:30:00+01'::timestamptz").await;
    assert_eq!(
        r.rows(),
        vec![&vec![Value::Timestamp {
            iso: "2026-03-29 03:30:00+02".into(),
            tz: None
        }]]
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn several_statements_give_several_result_sets() {
    let driver = pg16().await;
    let r = run(&driver, "SELECT 1; SELECT 'a', 'b'").await;
    assert_eq!(r.summary.as_ref().expect("query").rows_returned, 2);
    let columns = r.columns();
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[0].len(), 1);
    assert_eq!(columns[1].len(), 2);
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn writes_report_rows_affected() {
    let driver = pg16().await;
    // A TEMP table lives only for this connection, so tests leave nothing behind.
    let r = run(
        &driver,
        "CREATE TEMP TABLE t (x int); INSERT INTO t VALUES (1), (2), (3)",
    )
    .await;
    let summary = r.summary.expect("query");
    assert_eq!(summary.rows_returned, 0);
    assert_eq!(summary.rows_affected, 3);
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn server_errors_keep_the_servers_words_and_code() {
    let driver = pg16().await;
    let r = run(&driver, "SELECT * FROM table_that_does_not_exist").await;
    match r.summary {
        Err(DriverError::Server { code, message }) => {
            assert_eq!(code.as_deref(), Some("42P01"));
            assert!(message.contains("table_that_does_not_exist"), "{message}");
        }
        other => panic!("expected a server error, got {other:?}"),
    }
    // The connection is still usable after an error.
    assert!(driver.ping().await.is_ok());
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn dropping_the_receiver_stops_the_query() {
    let driver = pg16().await;
    let (tx, rx) = mpsc::channel(1);
    drop(rx); // the "UI" went away
    let result = driver
        .run_query("SELECT g FROM generate_series(1, 100000) AS g", tx)
        .await;
    assert!(matches!(result, Err(DriverError::ReceiverClosed)));
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn dropping_the_receiver_also_stops_the_server_query() {
    let driver = pg16().await;
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    let started = Instant::now();
    // Without cancel this would run for about 50 seconds (5000 rows x 10 ms).
    let result = driver
        .run_query(
            "SELECT g, pg_sleep(0.01) FROM generate_series(1, 5000) AS g",
            tx,
        )
        .await;
    assert!(matches!(result, Err(DriverError::ReceiverClosed)));
    // The connection is free again quickly, because the server stopped working.
    let ping = tokio::time::timeout(Duration::from_secs(5), driver.ping()).await;
    assert!(
        matches!(ping, Ok(Ok(_))),
        "connection stayed busy: {ping:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn cancel_stops_a_running_query() {
    // `Arc` lets two tasks share one driver: one runs the query, one cancels it.
    let driver = Arc::new(pg16().await);
    let background = Arc::clone(&driver);
    let started = Instant::now();
    // Would take 30 seconds if left alone.
    let query = tokio::spawn(async move { run(&background, "SELECT pg_sleep(30)").await });

    tokio::time::sleep(Duration::from_millis(300)).await;
    driver.cancel().await.expect("cancel");

    let run = tokio::time::timeout(Duration::from_secs(5), query)
        .await
        .expect("the query did not stop in time")
        .expect("task panicked");
    assert!(
        matches!(run.summary, Err(DriverError::Cancelled)),
        "got {:?}",
        run.summary
    );
    assert!(started.elapsed() < Duration::from_secs(5));

    // The same connection works afterwards, and a later query is not cancelled.
    let again = self::run(&driver, "SELECT 42").await;
    assert_eq!(again.rows(), vec![&vec![Value::Int(42)]]);
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn cancel_stops_a_query_that_is_producing_rows() {
    let driver = Arc::new(pg16().await);
    let background = Arc::clone(&driver);
    // 1000 rows at 50 ms each is about 50 seconds if left alone.
    let query = tokio::spawn(async move {
        run(
            &background,
            "SELECT g, pg_sleep(0.05) FROM generate_series(1, 1000) AS g",
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(500)).await;
    driver.cancel().await.expect("cancel");

    let run = tokio::time::timeout(Duration::from_secs(5), query)
        .await
        .expect("the query did not stop in time")
        .expect("task panicked");
    assert!(matches!(run.summary, Err(DriverError::Cancelled)));
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn cancel_when_nothing_is_running_does_nothing() {
    let driver = pg16().await;
    driver.cancel().await.expect("cancel on an idle connection");
    // A cancel sent while idle must not leak into the next query.
    let r = run(&driver, "SELECT pg_sleep(0.3), 1").await;
    assert_eq!(r.summary.expect("not cancelled").rows_returned, 1);
}
