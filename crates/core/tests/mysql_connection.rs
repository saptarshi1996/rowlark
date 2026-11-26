//! Connecting to MySQL and MariaDB: login, `ping`, the flavor, and the promise
//! that connecting leaves the session the way the server defaults have it (we
//! run the user's SQL as typed, so it must mean what it means in `mysql`).
//!
//! Needs `docker compose up -d`; run with:
//!
//!     cargo test -p rowlark-core --test mysql_connection -- --ignored

mod mysql_support;

use std::time::Duration;

use mysql_support::*;
use rowlark_core::{DriverError, Flavor, MySqlDriver, Value};

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn every_server_connects_and_says_which_flavor_it_is() {
    for (port, label) in SERVERS {
        let driver = connect_to(port).await;
        let info = driver
            .ping()
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        let expected = if label == "mariadb" {
            Flavor::MariaDb
        } else {
            Flavor::MySql
        };
        assert_eq!(info.flavor, expected, "{label}: {}", info.version);
        assert!(info.version.chars().next().unwrap().is_ascii_digit());
        // Asking twice works (the connection is reusable).
        assert_eq!(driver.ping().await.expect("second ping"), info, "{label}");
    }
}

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn a_wrong_password_is_reported_as_a_connection_error_in_the_servers_words() {
    let result = MySqlDriver::connect(&config(MYSQL8), "not-the-password").await;
    match result {
        Err(DriverError::Connection(message)) => {
            assert!(message.contains("Access denied"), "{message}");
        }
        other => panic!("expected a connection error, got {:?}", other.err()),
    }
}

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn a_closed_port_and_an_unreachable_host_fail_cleanly() {
    // Nothing listens on port 1: refused straight away.
    let refused = MySqlDriver::connect(&config(1), "rowlark").await;
    assert!(matches!(refused, Err(DriverError::Connection(_))));

    // A host that never answers: we give up after the timeout, not never.
    let mut cfg = config(3306);
    cfg.host = "10.255.255.1".to_string();
    cfg.connect_timeout = Duration::from_secs(1);
    let started = std::time::Instant::now();
    match MySqlDriver::connect(&cfg, "rowlark").await {
        Err(DriverError::Connection(message)) => {
            // Either our timeout or the network refusing at once is fine; a
            // hang is not.
            assert!(!message.is_empty());
            assert!(started.elapsed() < Duration::from_secs(5));
        }
        other => panic!("expected a connection error, got {:?}", other.err()),
    }
}

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn the_session_keeps_the_servers_own_defaults() {
    for (port, label) in SERVERS {
        let driver = connect_to(port).await;

        // sqlx would normally switch on PIPES_AS_CONCAT, which turns `a || b`
        // from "or" into "join text". We must not: `'a' || 'b'` is 0 (false
        // OR false) in default MySQL and MariaDB.
        let r = exec(&driver, "SELECT 'a' || 'b'").await;
        assert_eq!(r.scalar(), &Value::Int(0), "{label}: || must stay OR");
        let r = exec(
            &driver,
            "SELECT @@session.sql_mode = @@global.sql_mode, \
                    @@session.time_zone = @@global.time_zone",
        )
        .await;
        assert_eq!(
            r.rows()[0],
            &vec![Value::Int(1), Value::Int(1)],
            "{label}: sql_mode and time_zone are the server's own"
        );

        // Text travels as UTF-8, with the server's default collation for it
        // (what the `mysql` command line ends up with).
        let r = exec(
            &driver,
            "SELECT @@character_set_client, @@character_set_connection, \
                    @@character_set_results, @@collation_connection",
        )
        .await;
        let row = r.rows()[0].clone();
        for (i, expected) in ["utf8mb4", "utf8mb4", "utf8mb4"].iter().enumerate() {
            assert_eq!(row[i], Value::Text((*expected).into()), "{label}");
        }
        let default = exec(
            &driver,
            "SELECT default_collate_name FROM information_schema.character_sets \
             WHERE character_set_name = 'utf8mb4'",
        )
        .await;
        assert_eq!(&row[3], default.scalar(), "{label}: default collation");
    }
}

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn ping_reports_a_connection_the_server_has_closed() {
    let victim = connect_to(MYSQL8).await;
    let id = exec(&victim, "SELECT CONNECTION_ID()")
        .await
        .scalar()
        .clone();
    let Value::UInt(id) = id else {
        panic!("connection id should be an unsigned number, got {id:?}");
    };

    // A second connection (an admin, say) kills the first.
    let admin = connect_to(MYSQL8).await;
    exec(&admin, &format!("KILL CONNECTION {id}")).await;

    // The next ping says so, instead of pretending all is well.
    let mut error = None;
    for _ in 0..20 {
        if let Err(e) = victim.ping().await {
            error = Some(e);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        matches!(error, Some(DriverError::Connection(_))),
        "expected a connection error, got {error:?}"
    );
}

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn the_servers_own_error_text_and_sqlstate_come_through() {
    let driver = connect_to(MYSQL8).await;

    let r = run(&driver, "SELECT * FROM no_such_table").await;
    match r.result {
        Err(DriverError::Server { code, message }) => {
            assert_eq!(code.as_deref(), Some("42S02"));
            assert!(message.contains("no_such_table"), "{message}");
        }
        other => panic!("expected a server error, got {other:?}"),
    }

    let r = run(&driver, "SELEC 1").await;
    match r.result {
        Err(DriverError::Server { code, message }) => {
            assert_eq!(code.as_deref(), Some("42000"));
            assert!(message.contains("syntax"), "{message}");
        }
        other => panic!("expected a server error, got {other:?}"),
    }

    // A failed statement does not break the connection.
    assert_eq!(exec(&driver, "SELECT 7").await.scalar(), &Value::Int(7));
}

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn rows_arrive_in_batches_and_statements_report_what_they_changed() {
    for (port, label) in SERVERS {
        let driver = connect_to(port).await;
        let flavor = driver.ping().await.expect("ping").flavor;

        // The recursion limit has a different name on each server.
        let raise_limit = match flavor {
            Flavor::MariaDb => "SET SESSION max_recursive_iterations = 100000",
            _ => "SET SESSION cte_max_recursion_depth = 100000",
        };
        exec(&driver, raise_limit).await;
        let r = exec(
            &driver,
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2500) \
             SELECT i FROM n",
        )
        .await;
        assert_eq!(r.ok().rows_returned, 2500, "{label}");
        assert_eq!(r.batch_sizes(), vec![1000, 1000, 500], "{label}");
        assert_eq!(r.columns().len(), 1, "{label}: one header for one result");

        exec(&driver, "CREATE TEMPORARY TABLE t (a int)").await;
        let r = exec(&driver, "INSERT INTO t VALUES (1), (2), (3)").await;
        assert_eq!(r.ok().rows_affected, 3, "{label}");
        assert_eq!(r.ok().rows_returned, 0, "{label}");
        let r = exec(&driver, "UPDATE t SET a = a + 1 WHERE a > 1").await;
        assert_eq!(r.ok().rows_affected, 2, "{label}");
    }
}

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn several_statements_in_one_run_give_several_results() {
    let driver = connect_to(MYSQL8).await;
    let r = exec(&driver, "SELECT 1 AS a; SELECT 'x' AS b, 2 AS c").await;
    let columns = r.columns();
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[0][0].name, "a");
    assert_eq!(
        columns[1]
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["b", "c"]
    );
    assert_eq!(r.ok().rows_returned, 2);
}

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn a_query_that_fails_partway_still_delivers_the_rows_before_the_error() {
    let driver = connect_to(MYSQL8).await;
    // The first result is fine; the second statement fails.
    let r = run(&driver, "SELECT 1 AS ok; SELECT * FROM no_such_table").await;
    assert!(matches!(r.result, Err(DriverError::Server { .. })));
    assert_eq!(r.rows().len(), 1);
    assert_eq!(r.rows()[0], &vec![Value::Int(1)]);
}

#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn a_closed_receiver_stops_the_query_with_receiver_closed() {
    let driver = connect_to(MYSQL8).await;
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx);
    let result = driver.run_query("SELECT 1", tx).await;
    assert!(matches!(result, Err(DriverError::ReceiverClosed)));
}

/// KNOWN LIMIT, pinned so a future fix shows up: sqlx always asks the server
/// to report rows *matched* by an UPDATE (the `FOUND_ROWS` client flag), not
/// rows *changed*. The `mysql` command line reports changed rows, so there an
/// UPDATE that sets a column to the value it already has says 0. Here it says
/// 2. When this test fails because the number became 0, the limit is gone:
/// update this test and the note in PLAN.md.
#[tokio::test]
#[ignore = "needs the Docker MySQL/MariaDB servers"]
async fn update_counts_matched_rows_not_changed_rows_known_limit() {
    let driver = connect_to(MYSQL8).await;
    exec(&driver, "CREATE TEMPORARY TABLE same (a int)").await;
    exec(&driver, "INSERT INTO same VALUES (1), (1)").await;
    let r = exec(&driver, "UPDATE same SET a = 1").await;
    assert_eq!(r.ok().rows_affected, 2);
}
