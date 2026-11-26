//! Big results, a reader that cannot keep up, and `transaction_state` for
//! MySQL and MariaDB.
//!
//! The time limits are generous so a slow CI machine does not cause a false
//! alarm; they exist to catch something being accidentally quadratic or
//! unbounded, not to benchmark. Real numbers are printed with `--nocapture`.
//!
//!     cargo test -p rowlark-core --test mysql_streaming -- --ignored --nocapture

mod mysql_support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use mysql_support::*;
use rowlark_core::{Flavor, MySqlDriver, QueryEvent, TransactionState, Value};
use tokio::sync::mpsc;

/// `n` numbered rows, 1..=n. MySQL has no `generate_series`, so this counts
/// with a recursive query, whose depth limit has a different name on each server.
async fn counting_query(driver: &MySqlDriver, n: u64) -> String {
    let limit = match driver.ping().await.expect("ping").flavor {
        Flavor::MariaDb => "max_recursive_iterations",
        _ => "cte_max_recursion_depth",
    };
    exec(driver, &format!("SET SESSION {limit} = {}", n + 10)).await;
    format!(
        "WITH RECURSIVE n(g) AS (SELECT 1 UNION ALL SELECT g + 1 FROM n WHERE g < {n}) \
         SELECT g, CONCAT('row ', g) AS label FROM n"
    )
}

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn a_million_rows_stream_in_batches_without_loss() {
    for (port, label) in SERVERS {
        let driver = connect_to(port).await;
        let sql = counting_query(&driver, 1_000_000).await;
        let started = Instant::now();
        let (tx, mut rx) = mpsc::channel(4);
        let collect = async {
            let (mut rows, mut largest_batch, mut sum, mut next) = (0u64, 0usize, 0i64, 1i64);
            while let Some(event) = rx.recv().await {
                if let QueryEvent::Rows(batch) = event {
                    largest_batch = largest_batch.max(batch.len());
                    rows += batch.len() as u64;
                    for row in batch {
                        // Rows arrive in order, none missing or repeated.
                        let Value::Int(n) = row[0] else {
                            panic!("{label}: {row:?}")
                        };
                        assert_eq!(n, next, "{label}");
                        next += 1;
                        sum += n;
                    }
                }
            }
            (rows, largest_batch, sum)
        };
        let (result, (rows, largest_batch, sum)) =
            tokio::join!(driver.run_query(&sql, tx), collect);
        let elapsed = started.elapsed();
        assert_eq!(result.expect("query").rows_returned, 1_000_000, "{label}");
        assert_eq!(rows, 1_000_000, "{label}");
        assert_eq!(sum, 1_000_000 * 1_000_001 / 2, "{label}");
        // Never more than a batch at a time, so memory stays flat.
        assert_eq!(largest_batch, 1000, "{label}");
        eprintln!(
            "{label}: 1,000,000 rows in {elapsed:?} ({:.0} rows/s)",
            1e6 / elapsed.as_secs_f64()
        );
        assert!(elapsed < Duration::from_secs(120), "{label}: {elapsed:?}");
    }
}

/// If the UI reads slowly, the driver must wait for it instead of piling the
/// whole result up in memory.
#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn a_slow_reader_makes_the_driver_wait() {
    for (port, label) in SERVERS {
        let driver = Arc::new(connect_to(port).await);
        let sql = counting_query(&driver, 200_000).await;
        let producer = Arc::clone(&driver);
        let (tx, mut rx) = mpsc::channel(2);
        let query = tokio::spawn(async move { producer.run_query(&sql, tx).await });

        // Read one batch, then stop reading for a while.
        assert!(
            matches!(rx.recv().await, Some(QueryEvent::Columns(_))),
            "{label}"
        );
        assert!(
            matches!(rx.recv().await, Some(QueryEvent::Rows(_))),
            "{label}"
        );
        tokio::time::sleep(Duration::from_millis(700)).await;
        // 200 batches are waiting to be sent, yet the query is still running:
        // it is blocked on the full channel, holding at most a few batches.
        assert!(
            !query.is_finished(),
            "{label}: the driver ran ahead of a reader that was not reading"
        );

        // Resume reading: everything arrives.
        let mut rows = 1000u64;
        while let Some(event) = rx.recv().await {
            if let QueryEvent::Rows(batch) = event {
                rows += batch.len() as u64;
            }
        }
        assert_eq!(rows, 200_000, "{label}");
        assert_eq!(
            query.await.unwrap().unwrap().rows_returned,
            200_000,
            "{label}"
        );
    }
}

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn transaction_state_follows_begin_commit_and_rollback() {
    for (port, label) in SERVERS {
        let driver = connect_to(port).await;
        exec(&driver, "CREATE TEMPORARY TABLE t (a int) ENGINE=InnoDB").await;
        let state = || async { driver.transaction_state().await.expect("state") };

        assert_eq!(state().await, TransactionState::Idle, "{label}");
        exec(&driver, "BEGIN").await;
        assert_eq!(state().await, TransactionState::Open, "{label}");
        exec(&driver, "INSERT INTO t VALUES (1)").await;
        assert_eq!(state().await, TransactionState::Open, "{label}");
        exec(&driver, "COMMIT").await;
        assert_eq!(state().await, TransactionState::Idle, "{label}");

        exec(&driver, "START TRANSACTION").await;
        assert_eq!(state().await, TransactionState::Open, "{label}");
        exec(&driver, "ROLLBACK").await;
        assert_eq!(state().await, TransactionState::Idle, "{label}");
    }
}

/// Looking must not change anything: the check can be repeated, and the
/// user's open transaction is still theirs to roll back afterwards.
#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn checking_the_state_does_not_disturb_an_open_transaction() {
    for (port, label) in SERVERS {
        let driver = connect_to(port).await;
        exec(&driver, "CREATE TEMPORARY TABLE t (a int) ENGINE=InnoDB").await;
        exec(&driver, "BEGIN").await;
        exec(&driver, "INSERT INTO t VALUES (1), (2)").await;
        for _ in 0..3 {
            assert_eq!(
                driver.transaction_state().await.expect("state"),
                TransactionState::Open,
                "{label}"
            );
        }
        // The rows are still there to see, and still ours to undo.
        assert_eq!(
            exec(&driver, "SELECT COUNT(*) FROM t").await.scalar(),
            &Value::Int(2),
            "{label}"
        );
        exec(&driver, "ROLLBACK").await;
        assert_eq!(
            exec(&driver, "SELECT COUNT(*) FROM t").await.scalar(),
            &Value::Int(0),
            "{label}"
        );
    }
}

/// MySQL has no "aborted" state like Postgres: after an error the
/// transaction stays open and usable, so the answer is `Open`, never `Aborted`.
#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn an_error_inside_a_transaction_leaves_it_open_not_aborted() {
    for (port, label) in SERVERS {
        let driver = connect_to(port).await;
        exec(&driver, "CREATE TEMPORARY TABLE t (a int) ENGINE=InnoDB").await;
        exec(&driver, "BEGIN").await;
        exec(&driver, "INSERT INTO t VALUES (1)").await;
        assert!(
            run(&driver, "SELECT * FROM no_such_table")
                .await
                .result
                .is_err()
        );
        assert_eq!(
            driver.transaction_state().await.expect("state"),
            TransactionState::Open,
            "{label}"
        );
        // And it still works: the earlier insert is intact and can be kept.
        exec(&driver, "COMMIT").await;
        assert_eq!(
            exec(&driver, "SELECT COUNT(*) FROM t").await.scalar(),
            &Value::Int(1),
            "{label}"
        );
    }
}

/// Some statements (CREATE TABLE and friends) commit the open transaction by
/// themselves. The state follows what the server really did.
#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn a_statement_that_commits_implicitly_ends_the_open_transaction() {
    for (port, label) in SERVERS {
        let driver = connect_to(port).await;
        exec(&driver, "BEGIN").await;
        assert_eq!(
            driver.transaction_state().await.expect("state"),
            TransactionState::Open,
            "{label}"
        );
        exec(&driver, "CREATE TEMPORARY TABLE t (a int)").await;
        // A temporary table does not commit; a real one does.
        exec(
            &driver,
            "CREATE TABLE IF NOT EXISTS rowlark_implicit_commit (a int)",
        )
        .await;
        assert_eq!(
            driver.transaction_state().await.expect("state"),
            TransactionState::Idle,
            "{label}"
        );
        exec(&driver, "DROP TABLE rowlark_implicit_commit").await;
    }
}

/// With `autocommit` off, MySQL opens a transaction by itself at the first
/// statement, and it stays open until COMMIT or ROLLBACK: locks may be held,
/// which is exactly what the UI wants to warn about.
///
/// KNOWN QUIRK, pinned: asking is itself a savepoint, and with autocommit off
/// that opens the implicit transaction, so right after a COMMIT the answer is
/// still `Open` (it would be `Open` after the user's next statement anyway).
#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn with_autocommit_off_the_first_statement_opens_a_transaction() {
    for (port, label) in SERVERS {
        let driver = connect_to(port).await;
        exec(&driver, "CREATE TEMPORARY TABLE t (a int) ENGINE=InnoDB").await;
        exec(&driver, "SET autocommit = 0").await;
        exec(&driver, "INSERT INTO t VALUES (1)").await;
        assert_eq!(
            driver.transaction_state().await.expect("state"),
            TransactionState::Open,
            "{label}"
        );
        exec(&driver, "COMMIT").await;
        assert_eq!(
            driver.transaction_state().await.expect("state"),
            TransactionState::Open,
            "{label}: known quirk, see the comment above"
        );
        // Nothing is lost by asking: a ROLLBACK ends it, and the committed
        // row is still there.
        exec(&driver, "ROLLBACK").await;
        assert_eq!(
            exec(&driver, "SELECT COUNT(*) FROM t").await.scalar(),
            &Value::Int(1),
            "{label}"
        );
    }
}

/// The probe must not trample the user's own savepoints, and must not stand
/// in the way of a statement that really is wrong.
#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn asking_leaves_the_users_savepoints_alone() {
    for (port, label) in SERVERS {
        let driver = connect_to(port).await;
        exec(&driver, "CREATE TEMPORARY TABLE t (a int) ENGINE=InnoDB").await;
        exec(&driver, "BEGIN").await;
        exec(&driver, "INSERT INTO t VALUES (1)").await;
        exec(&driver, "SAVEPOINT mine").await;
        exec(&driver, "INSERT INTO t VALUES (2)").await;
        assert_eq!(
            driver.transaction_state().await.expect("state"),
            TransactionState::Open,
            "{label}"
        );
        // Their savepoint still works and rolls back exactly to where they set it.
        exec(&driver, "ROLLBACK TO SAVEPOINT mine").await;
        assert_eq!(
            exec(&driver, "SELECT COUNT(*) FROM t").await.scalar(),
            &Value::Int(1),
            "{label}"
        );
        exec(&driver, "ROLLBACK").await;
        // A real syntax error is still reported as an error, not mistaken for
        // "no transaction".
        assert!(run(&driver, "SAVEPOINT").await.result.is_err());
        assert_eq!(
            driver.transaction_state().await.expect("state"),
            TransactionState::Idle,
            "{label}"
        );
    }
}

/// While a query runs on the connection we cannot ask it anything, and we do
/// not wait for it: the answer is `Busy` straight away.
#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn the_state_is_busy_while_a_query_runs() {
    let driver = Arc::new(connect_to(MYSQL8).await);
    let producer = Arc::clone(&driver);
    let (tx, mut rx) = mpsc::channel(4);
    let query = tokio::spawn(async move { producer.run_query("SELECT SLEEP(1.5)", tx).await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let started = Instant::now();
    assert_eq!(
        driver.transaction_state().await.expect("state"),
        TransactionState::Busy
    );
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "it must not wait"
    );

    while rx.recv().await.is_some() {}
    query.await.unwrap().expect("query");
    assert_eq!(
        driver.transaction_state().await.expect("state"),
        TransactionState::Idle
    );
}

/// If the connection is gone, asking says so, instead of guessing "idle".
#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn a_closed_connection_is_an_error_not_a_guess() {
    let victim = connect_to(MYSQL8).await;
    let Value::UInt(id) = exec(&victim, "SELECT CONNECTION_ID()")
        .await
        .scalar()
        .clone()
    else {
        panic!("connection id should be an unsigned number");
    };
    let admin = connect_to(MYSQL8).await;
    exec(&admin, &format!("KILL CONNECTION {id}")).await;

    let mut error = None;
    for _ in 0..20 {
        match victim.transaction_state().await {
            Err(e) => {
                error = Some(e);
                break;
            }
            Ok(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    assert!(
        matches!(error, Some(rowlark_core::DriverError::Connection(_))),
        "expected a connection error, got {error:?}"
    );
}
