//! Server notices and transaction state.
//!
//! Needs `docker compose up -d`; run with:
//!
//!     cargo test -p rowlark-core --test postgres_session -- --ignored

use std::sync::Arc;
use std::time::{Duration, Instant};

use rowlark_core::{
    ConnectionConfig, Driver, DriverError, Notice, NoticeSeverity, PostgresDriver, QueryEvent,
    QuerySummary, TransactionState,
};
use tokio::sync::mpsc;

struct Run {
    result: Result<QuerySummary, DriverError>,
    events: Vec<QueryEvent>,
}

impl Run {
    fn notices(&self) -> Vec<&Notice> {
        self.events
            .iter()
            .filter_map(|e| match e {
                QueryEvent::Notice(n) => Some(n),
                _ => None,
            })
            .collect()
    }

    fn messages(&self) -> Vec<&str> {
        self.notices().iter().map(|n| n.message.as_str()).collect()
    }

    /// A short label for each event, to check ordering.
    fn shape(&self) -> Vec<String> {
        self.events
            .iter()
            .map(|e| match e {
                QueryEvent::Columns(_) => "columns".to_string(),
                QueryEvent::Rows(rows) => format!("rows({})", rows.len()),
                QueryEvent::Notice(n) => format!("notice({})", n.message),
            })
            .collect()
    }
}

async fn run(driver: &PostgresDriver, sql: &str) -> Run {
    let (tx, mut rx) = mpsc::channel(4);
    let collect = async {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    };
    let (result, events) = tokio::join!(driver.run_query(sql, tx), collect);
    Run { result, events }
}

async fn connect() -> PostgresDriver {
    let config = ConnectionConfig::new("127.0.0.1", 54316, "rowlark", "rowlark_test");
    PostgresDriver::connect(&config, "rowlark")
        .await
        .expect("is `docker compose up -d` running?")
}

// ---------------------------------------------------------------- notices --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn notices_from_the_server_reach_the_caller() {
    let driver = connect().await;

    let r = run(
        &driver,
        "DO $$ BEGIN RAISE NOTICE 'hello %', 42; RAISE WARNING 'careful'; RAISE INFO 'fyi'; END $$",
    )
    .await;
    r.result.as_ref().expect("query");
    let notices = r.notices();
    assert_eq!(notices.len(), 3, "{:?}", r.shape());
    assert_eq!(
        (notices[0].severity, notices[0].message.as_str()),
        (NoticeSeverity::Notice, "hello 42")
    );
    assert_eq!(
        (notices[1].severity, notices[1].message.as_str()),
        (NoticeSeverity::Warning, "careful")
    );
    assert_eq!(
        (notices[2].severity, notices[2].message.as_str()),
        (NoticeSeverity::Info, "fyi")
    );

    // The notices Postgres sends by itself.
    let r = run(&driver, "DROP TABLE IF EXISTS definitely_not_a_table").await;
    r.result.as_ref().expect("query");
    assert_eq!(r.notices().len(), 1);
    assert!(
        r.messages()[0].contains("does not exist, skipping"),
        "{:?}",
        r.messages()
    );

    // Debug messages only reach us when the session asks for them.
    run(&driver, "SET client_min_messages = debug1")
        .await
        .result
        .expect("set");
    let r = run(&driver, "DO $$ BEGIN RAISE DEBUG 'dbg'; END $$").await;
    assert!(
        r.notices()
            .iter()
            .any(|n| n.severity == NoticeSeverity::Debug && n.message == "dbg"),
        "{:?}",
        r.shape()
    );
    run(&driver, "SET client_min_messages = notice")
        .await
        .result
        .expect("reset");

    // A query with nothing to say sends no notices.
    assert!(run(&driver, "SELECT 1").await.notices().is_empty());
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn notices_arrive_in_the_order_the_server_sent_them() {
    let driver = connect().await;
    let r = run(
        &driver,
        "SELECT 1; DO $$ BEGIN RAISE NOTICE 'middle'; END $$; SELECT 2",
    )
    .await;
    r.result.as_ref().expect("query");
    assert_eq!(
        r.shape(),
        ["columns", "rows(1)", "notice(middle)", "columns", "rows(1)"]
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn notices_raised_between_rows_arrive_with_all_the_rows() {
    let driver = connect().await;
    run(
        &driver,
        "CREATE OR REPLACE FUNCTION pg_temp.noisy(i int) RETURNS int LANGUAGE plpgsql AS $$ \
         BEGIN RAISE NOTICE 'at %', i; RETURN i; END $$",
    )
    .await
    .result
    .expect("create function");
    // The function raises a notice for each row while the rows are being produced.
    let r = run(
        &driver,
        "SELECT pg_temp.noisy(g) FROM generate_series(1, 5) g",
    )
    .await;
    r.result.as_ref().expect("query");
    assert_eq!(r.notices().len(), 5, "{:?}", r.shape());
    let rows: usize = r
        .events
        .iter()
        .map(|e| {
            if let QueryEvent::Rows(rows) = e {
                rows.len()
            } else {
                0
            }
        })
        .sum();
    assert_eq!(rows, 5, "{:?}", r.shape());
    // A notice never arrives before the columns it belongs to are known... except
    // those the server sent first; what matters is nothing was lost or doubled.
    let mut messages: Vec<String> = r.notices().iter().map(|n| n.message.clone()).collect();
    messages.sort();
    assert_eq!(messages, ["at 1", "at 2", "at 3", "at 4", "at 5"]);
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn notices_sent_before_an_error_are_not_lost() {
    let driver = connect().await;
    let r = run(
        &driver,
        "DO $$ BEGIN RAISE NOTICE 'before'; RAISE EXCEPTION 'boom'; END $$",
    )
    .await;
    assert!(matches!(r.result, Err(DriverError::Server { .. })));
    assert_eq!(r.messages(), ["before"]);
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn notices_do_not_leak_between_queries_running_at_the_same_time() {
    // Two connections each raise 40 notices tagged with their own letter, at
    // the same time on the same runtime. Each must see only its own.
    let a = Arc::new(connect().await);
    let b = Arc::new(connect().await);
    let sql = |tag: &str| {
        format!(
            "DO $$ BEGIN FOR i IN 1..40 LOOP RAISE NOTICE '{tag} %', i; PERFORM pg_sleep(0.005); END LOOP; END $$"
        )
    };
    // Named variables, so the text outlives the queries that borrow it.
    let (sql_a, sql_b) = (sql("A"), sql("B"));
    let (ra, rb) = tokio::join!(run(&a, &sql_a), run(&b, &sql_b));
    for (r, tag) in [(&ra, "A"), (&rb, "B")] {
        r.result.as_ref().expect("query");
        assert_eq!(r.notices().len(), 40, "{tag}");
        assert!(
            r.messages()
                .iter()
                .all(|m| m.starts_with(&format!("{tag} "))),
            "{tag} saw someone else's notice: {:?}",
            r.messages()
        );
    }
}

// ------------------------------------------------------ transaction state --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn transaction_state_follows_what_the_user_does() {
    let driver = connect().await;
    assert_eq!(
        driver.transaction_state().await.unwrap(),
        TransactionState::Idle
    );

    run(&driver, "BEGIN").await.result.expect("begin");
    assert_eq!(
        driver.transaction_state().await.unwrap(),
        TransactionState::Open
    );
    // Still open after more work.
    run(&driver, "SELECT 1").await.result.expect("select");
    assert_eq!(
        driver.transaction_state().await.unwrap(),
        TransactionState::Open
    );

    run(&driver, "COMMIT").await.result.expect("commit");
    assert_eq!(
        driver.transaction_state().await.unwrap(),
        TransactionState::Idle
    );

    // BEGIN and more work in one run.
    run(&driver, "BEGIN; SELECT 1")
        .await
        .result
        .expect("begin and select");
    assert_eq!(
        driver.transaction_state().await.unwrap(),
        TransactionState::Open
    );
    run(&driver, "ROLLBACK").await.result.expect("rollback");
    assert_eq!(
        driver.transaction_state().await.unwrap(),
        TransactionState::Idle
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_failed_statement_inside_a_transaction_is_reported_as_aborted() {
    let driver = connect().await;
    run(&driver, "BEGIN").await.result.expect("begin");
    let failed = run(&driver, "SELECT 1/0").await;
    assert!(matches!(failed.result, Err(DriverError::Server { .. })));
    assert_eq!(
        driver.transaction_state().await.unwrap(),
        TransactionState::Aborted
    );

    run(&driver, "ROLLBACK").await.result.expect("rollback");
    assert_eq!(
        driver.transaction_state().await.unwrap(),
        TransactionState::Idle
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn transaction_state_does_not_wait_for_a_running_query() {
    let driver = Arc::new(connect().await);
    let background = Arc::clone(&driver);
    let slow = tokio::spawn(async move { run(&background, "SELECT pg_sleep(2)").await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let started = Instant::now();
    assert_eq!(
        driver.transaction_state().await.unwrap(),
        TransactionState::Busy
    );
    assert!(started.elapsed() < Duration::from_millis(500));

    slow.await.expect("task").result.expect("slow query");
    assert_eq!(
        driver.transaction_state().await.unwrap(),
        TransactionState::Idle
    );
}
