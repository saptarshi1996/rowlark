//! Session state that must survive from one `run_query` to the next.
//!
//! The UI keeps one connection per tab, so whatever a user does in one run
//! (SET, BEGIN, temp tables, cursors...) must still be in effect in the next.
//!
//! Needs `docker compose up -d`; run with:
//!
//!     cargo test -p rowlark-core --test postgres_session_state -- --ignored

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{exec, pg16, run};
use rowlark_core::{Driver, DriverError, TransactionState, Value};

fn text(s: &str) -> Value {
    Value::Text(s.to_string())
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn set_commands_stay_in_effect_for_later_runs() {
    let driver = pg16().await;
    exec(&driver, "SET TIME ZONE 'Asia/Kolkata'").await;
    exec(&driver, "SET search_path TO pg_catalog").await;

    assert_eq!(
        exec(&driver, "SHOW timezone").await.scalar(),
        &text("Asia/Kolkata")
    );
    assert_eq!(
        exec(&driver, "SHOW search_path").await.scalar(),
        &text("pg_catalog")
    );
    // The setting really changes results, not only what SHOW says.
    let offset = exec(
        &driver,
        "SELECT '2026-01-01 00:00:00+00'::timestamptz::text",
    )
    .await;
    assert_eq!(offset.scalar(), &text("2026-01-01 05:30:00+05:30"));

    // RESET undoes it, again across runs.
    exec(&driver, "RESET TIME ZONE").await;
    assert_ne!(
        exec(&driver, "SHOW timezone").await.scalar(),
        &text("Asia/Kolkata")
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn settings_do_not_leak_into_another_connection() {
    let first = pg16().await;
    let second = pg16().await;
    exec(&first, "SET TIME ZONE 'Pacific/Auckland'").await;
    assert_ne!(
        exec(&second, "SHOW timezone").await.scalar(),
        &text("Pacific/Auckland")
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_transaction_can_span_several_runs_and_commit() {
    let driver = pg16().await;
    let other = pg16().await;
    exec(
        &driver,
        "DROP SCHEMA IF EXISTS state_commit CASCADE; CREATE SCHEMA state_commit; \
         CREATE TABLE state_commit.t (n int)",
    )
    .await;

    exec(&driver, "BEGIN").await;
    assert_eq!(
        driver.transaction_state().await.unwrap(),
        TransactionState::Open
    );
    exec(&driver, "INSERT INTO state_commit.t VALUES (1), (2)").await;

    // Inside the transaction we see our rows; another connection does not.
    let mine = exec(&driver, "SELECT count(*) FROM state_commit.t").await;
    assert_eq!(mine.scalar(), &Value::Int(2));
    let theirs = exec(&other, "SELECT count(*) FROM state_commit.t").await;
    assert_eq!(theirs.scalar(), &Value::Int(0));

    exec(&driver, "COMMIT").await;
    assert_eq!(
        driver.transaction_state().await.unwrap(),
        TransactionState::Idle
    );
    let after = exec(&other, "SELECT count(*) FROM state_commit.t").await;
    assert_eq!(after.scalar(), &Value::Int(2));

    exec(&driver, "DROP SCHEMA state_commit CASCADE").await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_rolled_back_transaction_leaves_nothing_behind() {
    let driver = pg16().await;
    let other = pg16().await;
    exec(
        &driver,
        "DROP SCHEMA IF EXISTS state_rollback CASCADE; CREATE SCHEMA state_rollback; \
         CREATE TABLE state_rollback.t (n int)",
    )
    .await;

    exec(&driver, "BEGIN").await;
    exec(&driver, "INSERT INTO state_rollback.t VALUES (1)").await;
    exec(&driver, "ROLLBACK").await;

    let count = exec(&other, "SELECT count(*) FROM state_rollback.t").await;
    assert_eq!(count.scalar(), &Value::Int(0));
    assert_eq!(
        driver.transaction_state().await.unwrap(),
        TransactionState::Idle
    );

    exec(&driver, "DROP SCHEMA state_rollback CASCADE").await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn savepoints_work_across_runs() {
    let driver = pg16().await;
    exec(&driver, "CREATE TEMP TABLE sp (n int)").await;
    exec(&driver, "BEGIN").await;
    exec(&driver, "INSERT INTO sp VALUES (1)").await;
    exec(&driver, "SAVEPOINT a").await;
    exec(&driver, "INSERT INTO sp VALUES (2)").await;

    // A failure inside the transaction aborts it...
    assert!(run(&driver, "SELECT 1/0").await.result.is_err());
    assert_eq!(
        driver.transaction_state().await.unwrap(),
        TransactionState::Aborted
    );
    // ...and rolling back to the savepoint (in a later run) revives it.
    exec(&driver, "ROLLBACK TO SAVEPOINT a").await;
    assert_eq!(
        driver.transaction_state().await.unwrap(),
        TransactionState::Open
    );
    exec(&driver, "COMMIT").await;

    let count = exec(&driver, "SELECT count(*) FROM sp").await;
    assert_eq!(count.scalar(), &Value::Int(1));
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn temp_tables_prepared_statements_and_locks_outlive_a_run() {
    let driver = pg16().await;
    let other = pg16().await;

    exec(&driver, "CREATE TEMP TABLE keepme AS SELECT 5 AS n").await;
    assert_eq!(
        exec(&driver, "SELECT n FROM keepme").await.scalar(),
        &Value::Int(5)
    );

    exec(&driver, "PREPARE myplan AS SELECT 6 AS n").await;
    assert_eq!(
        exec(&driver, "EXECUTE myplan").await.scalar(),
        &Value::Int(6)
    );

    // A session-level advisory lock is held until the connection ends, so
    // another connection cannot take it.
    exec(&driver, "SELECT pg_advisory_lock(424242)").await;
    let tried = exec(&other, "SELECT pg_try_advisory_lock(424242)").await;
    assert_eq!(tried.scalar(), &Value::Bool(false));
    exec(&driver, "SELECT pg_advisory_unlock(424242)").await;
    let tried = exec(&other, "SELECT pg_try_advisory_lock(424242)").await;
    assert_eq!(tried.scalar(), &Value::Bool(true));
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_cursor_the_user_declares_keeps_its_place_between_runs() {
    let driver = pg16().await;
    exec(&driver, "BEGIN").await;
    exec(
        &driver,
        "DECLARE c CURSOR FOR SELECT generate_series(1, 6) AS n",
    )
    .await;

    let first = exec(&driver, "FETCH 2 FROM c").await;
    assert_eq!(
        first.rows(),
        vec![&vec![Value::Int(1)], &vec![Value::Int(2)]]
    );
    let second = exec(&driver, "FETCH 2 FROM c").await;
    assert_eq!(
        second.rows(),
        vec![&vec![Value::Int(3)], &vec![Value::Int(4)]]
    );
    exec(&driver, "ROLLBACK").await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn cancelling_inside_a_transaction_leaves_it_aborted_until_rollback() {
    let driver = Arc::new(pg16().await);
    exec(&driver, "BEGIN").await;

    let background = Arc::clone(&driver);
    let query = tokio::spawn(async move { run(&background, "SELECT pg_sleep(30)").await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    driver.cancel().await.expect("cancel");
    let cancelled = tokio::time::timeout(Duration::from_secs(5), query)
        .await
        .expect("the query did not stop")
        .expect("task panicked");
    assert!(
        matches!(cancelled.result, Err(DriverError::Cancelled)),
        "got {:?}",
        cancelled.result
    );

    // Postgres aborts the whole transaction when a statement is cancelled.
    assert_eq!(
        driver.transaction_state().await.unwrap(),
        TransactionState::Aborted
    );
    // Everything fails with 25P02 (in failed transaction) until ROLLBACK.
    match run(&driver, "SELECT 1").await.result {
        Err(DriverError::Server { code, .. }) => assert_eq!(code.as_deref(), Some("25P02")),
        other => panic!("expected a 25P02 error, got {other:?}"),
    }
    exec(&driver, "ROLLBACK").await;
    assert_eq!(
        driver.transaction_state().await.unwrap(),
        TransactionState::Idle
    );
    assert_eq!(exec(&driver, "SELECT 1").await.scalar(), &Value::Int(1));
}
