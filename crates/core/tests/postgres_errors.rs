//! What happens when things go wrong: errors in the middle of a result,
//! permissions, bad logins, and a connection that dies.
//!
//! Needs `docker compose up -d`; run with:
//!
//!     cargo test -p rowlark-core --test postgres_errors -- --ignored

mod common;

use common::*;
use rowlark_core::{DriverError, Value};

#[tokio::test]
#[ignore = "needs docker compose"]
async fn rows_read_before_an_error_are_not_thrown_away() {
    let driver = pg16().await;
    // Fails on row 1500. By then 1000 rows were sent as a batch and 499 more
    // were read but not yet sent. Those must still reach the caller.
    let r = run(
        &driver,
        "SELECT g, 1 / (g - 1500) FROM generate_series(1, 2000) AS g",
    )
    .await;
    match &r.result {
        Err(DriverError::Server { code, .. }) => assert_eq!(code.as_deref(), Some("22012")),
        other => panic!("expected division by zero, got {other:?}"),
    }
    assert_eq!(r.rows().len(), 1499, "batches: {:?}", r.batch_sizes());
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn an_earlier_result_set_survives_a_later_statement_failing() {
    let driver = pg16().await;
    let r = run(&driver, "SELECT 1 AS first; SELECT 1/0; SELECT 3").await;
    assert!(matches!(r.result, Err(DriverError::Server { .. })));
    // The first statement's result arrived; the third never ran.
    assert_eq!(r.columns().len(), 1);
    assert_eq!(r.rows(), vec![&vec![Value::Int(1)]]);
}

// ------------------------------------------------------------ permissions --

/// Rowlark never decides what is allowed: the server does, and its words reach
/// the caller unchanged. Checked on every Postgres version.
#[tokio::test]
#[ignore = "needs docker compose"]
async fn permissions_are_the_servers_and_its_errors_arrive_verbatim() {
    use rowlark_core::{ConnectionConfig, Driver, PostgresDriver};

    for port in PG_PORTS {
        let admin = connect_to(port).await;
        exec(
            &admin,
            "DROP SCHEMA IF EXISTS perm_test CASCADE; \
             CREATE SCHEMA perm_test; \
             CREATE TABLE perm_test.t (id int, v text); \
             INSERT INTO perm_test.t VALUES (1, 'x'), (2, 'y'); \
             DO $$ BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'rowlark_ro') \
                 THEN CREATE ROLE rowlark_ro LOGIN PASSWORD 'ro'; END IF; END $$; \
             GRANT USAGE ON SCHEMA perm_test TO rowlark_ro; \
             GRANT SELECT ON perm_test.t TO rowlark_ro",
        )
        .await;

        let config = ConnectionConfig::new("127.0.0.1", port, "rowlark_ro", "rowlark_test");
        let reader = PostgresDriver::connect(&config, "ro")
            .await
            .expect("reader");

        // Allowed: a SELECT.
        let r = exec(&reader, "SELECT count(*) FROM perm_test.t").await;
        assert_eq!(r.scalar(), &Value::Int(2), "port {port}");

        // Not allowed: everything else. Rowlark sends each statement as typed
        // (including the ones a confirm prompt would ask about: TRUNCATE and a
        // DELETE without WHERE) and the *server* refuses, in its own words.
        for (sql, expected_code, expected_text) in [
            (
                "INSERT INTO perm_test.t VALUES (3, 'z')",
                "42501",
                "permission denied for table t",
            ),
            (
                "UPDATE perm_test.t SET v = 'q'",
                "42501",
                "permission denied for table t",
            ),
            (
                "DELETE FROM perm_test.t",
                "42501",
                "permission denied for table t",
            ),
            (
                "TRUNCATE perm_test.t",
                "42501",
                "permission denied for table t",
            ),
            (
                "DROP TABLE perm_test.t",
                "42501",
                "must be owner of table t",
            ),
            (
                "CREATE TABLE perm_test.n (a int)",
                "42501",
                "permission denied for schema perm_test",
            ),
            (
                "SELECT * FROM pg_authid",
                "42501",
                "permission denied for table pg_authid",
            ),
        ] {
            match run(&reader, sql).await.result {
                Err(DriverError::Server { code, message }) => {
                    assert_eq!(code.as_deref(), Some(expected_code), "port {port}: {sql}");
                    assert!(
                        message.contains(expected_text),
                        "port {port}: {sql}\n  got: {message}"
                    );
                }
                other => panic!("port {port}: {sql}\n  expected a permission error, got {other:?}"),
            }
            // The connection is fine afterwards.
            assert!(reader.ping().await.is_ok());
        }

        // The data is untouched, and introspection works for the limited user too.
        let r = exec(&admin, "SELECT count(*) FROM perm_test.t").await;
        assert_eq!(r.scalar(), &Value::Int(2));
        let tables = reader.list_tables("perm_test").await.expect("tables");
        assert_eq!(tables.len(), 1);
        assert_eq!(
            reader
                .describe_table("perm_test", "t")
                .await
                .unwrap()
                .columns
                .len(),
            2
        );

        // And Rowlark itself imposes no limits on someone who IS allowed: a
        // DELETE with no WHERE goes straight through (the confirm prompt is
        // the UI's job and never blocks).
        let r = exec(&admin, "DELETE FROM perm_test.t").await;
        assert_eq!(r.ok().rows_affected, 2);

        exec(&admin, "DROP SCHEMA perm_test CASCADE").await;
    }
}

// ----------------------------------------------------------------- logins --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn bad_logins_give_the_servers_message() {
    use rowlark_core::{ConnectionConfig, PostgresDriver};

    // Unknown user (the server says "password authentication failed" so as not
    // to reveal which users exist) and wrong password.
    for (user, password) in [("nobody_here", "x"), ("rowlark", "wrong")] {
        let config = ConnectionConfig::new("127.0.0.1", PG16, user, "rowlark_test");
        match PostgresDriver::connect(&config, password).await {
            Err(DriverError::Connection(message)) => {
                assert!(
                    message.contains("authentication failed"),
                    "{user}: {message}"
                );
            }
            other => panic!("{user}: expected a login error, got {:?}", other.err()),
        }
    }

    // A database that does not exist.
    let config = ConnectionConfig::new("127.0.0.1", PG16, "rowlark", "no_such_database");
    match PostgresDriver::connect(&config, "rowlark").await {
        Err(DriverError::Connection(message)) => {
            assert!(
                message.contains("database \"no_such_database\" does not exist"),
                "{message}"
            );
        }
        other => panic!("expected a missing database error, got {:?}", other.err()),
    }

    // Nothing listening on the port: refused straight away, not a hang.
    let config = ConnectionConfig::new("127.0.0.1", 1, "rowlark", "rowlark_test");
    assert!(matches!(
        PostgresDriver::connect(&config, "rowlark").await,
        Err(DriverError::Connection(_))
    ));
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_password_full_of_special_characters_works() {
    use rowlark_core::{ConnectionConfig, PostgresDriver};

    // Spaces, quotes, @ : / # and non-ASCII text, none of which may be
    // mangled on the way to the server.
    let password = "p@ss:w/rd #1 ' \"q\" ü😀";
    let admin = pg16().await;
    exec(
        &admin,
        "DO $$ BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'rowlark_pw') THEN \
             CREATE ROLE rowlark_pw LOGIN PASSWORD 'p@ss:w/rd #1 '' \"q\" ü😀'; \
         END IF; END $$",
    )
    .await;

    let config = ConnectionConfig::new("127.0.0.1", PG16, "rowlark_pw", "rowlark_test");
    let driver = PostgresDriver::connect(&config, password)
        .await
        .expect("direct");
    assert_eq!(
        exec(&driver, "SELECT current_user::text").await.scalar(),
        &Value::Text("rowlark_pw".into())
    );

    // The same login written as a URL, with everything percent-encoded.
    let url = "postgres://rowlark_pw:p%40ss%3Aw%2Frd%20%231%20%27%20%22q%22%20%C3%BC%F0%9F%98%80\
               @127.0.0.1:54316/rowlark_test?sslmode=disable";
    let (config, from_url) = ConnectionConfig::from_postgres_url(url).expect("url");
    assert_eq!(from_url.as_deref(), Some(password));
    PostgresDriver::connect(&config, password)
        .await
        .expect("via url");
}

// ------------------------------------------------------- dying connections --

/// The server's backend number for a driver's main connection.
async fn backend_pid(driver: &rowlark_core::PostgresDriver) -> i64 {
    match exec(driver, "SELECT pg_backend_pid()::int8").await.scalar() {
        Value::Int(pid) => *pid,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_killed_connection_gives_errors_not_hangs() {
    use rowlark_core::Driver;
    use std::time::Duration;
    use tokio::time::timeout;

    let victim = pg16().await;
    let pid = backend_pid(&victim).await;
    exec(
        &pg16().await,
        &format!("SELECT pg_terminate_backend({pid})"),
    )
    .await;

    // Every later call fails quickly and clearly; none hangs.
    for _ in 0..2 {
        let r = timeout(Duration::from_secs(5), run(&victim, "SELECT 1"))
            .await
            .expect("query on a dead connection hung");
        assert!(r.result.is_err(), "{:?}", r.result);
    }
    let ping = timeout(Duration::from_secs(5), victim.ping())
        .await
        .expect("ping hung");
    assert!(ping.is_err());
    // Cancelling something that is already dead does not hang or panic.
    let cancel = timeout(Duration::from_secs(5), victim.cancel())
        .await
        .expect("cancel hung");
    drop(cancel);
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_connection_killed_mid_query_ends_the_query() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::time::timeout;

    let victim = Arc::new(pg16().await);
    let pid = backend_pid(&victim).await;
    let background = Arc::clone(&victim);
    let started = Instant::now();
    let query = tokio::spawn(async move { run(&background, "SELECT pg_sleep(30)").await });

    tokio::time::sleep(Duration::from_millis(300)).await;
    exec(
        &pg16().await,
        &format!("SELECT pg_terminate_backend({pid})"),
    )
    .await;

    let r = timeout(Duration::from_secs(5), query)
        .await
        .expect("the query did not end after its connection was killed")
        .expect("task");
    assert!(r.result.is_err(), "{:?}", r.result);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn introspection_recovers_when_its_connection_dies() {
    use rowlark_core::{ConnectionConfig, Driver, PostgresDriver};

    // A private application name lets us find this driver's connections
    // among everyone else's.
    let mut config = ConnectionConfig::new("127.0.0.1", PG16, "rowlark", "rowlark_test");
    config.application_name = "meta-recovery-test".into();
    let driver = PostgresDriver::connect(&config, "rowlark")
        .await
        .expect("connect");
    let main_pid = backend_pid(&driver).await;
    driver
        .list_schemas()
        .await
        .expect("opens the introspection connection");

    // Kill the introspection connection (the other one with our name).
    let admin = pg16().await;
    exec(
        &admin,
        &format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE application_name = 'meta-recovery-test' AND pid <> {main_pid}"
        ),
    )
    .await;

    // The next introspection call quietly reconnects.
    let schemas = driver
        .list_schemas()
        .await
        .expect("introspection after its connection died");
    assert!(schemas.contains(&"public".to_string()));
    // And the main connection never noticed.
    assert_eq!(exec(&driver, "SELECT 1").await.scalar(), &Value::Int(1));
}

// ------------------------------------------------------------ concurrency --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn queries_sharing_a_connection_take_turns() {
    use rowlark_core::Driver;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let driver = Arc::new(pg16().await);
    let (d1, d2, d3) = (driver.clone(), driver.clone(), driver.clone());
    let started = Instant::now();
    let slow = tokio::spawn(async move { run(&d1, "SELECT pg_sleep(0.5), 'slow'").await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let quick = tokio::spawn(async move { run(&d2, "SELECT 'quick'").await });
    let ping = tokio::spawn(async move { d3.ping().await });

    let (slow, quick, ping) = (
        slow.await.unwrap(),
        quick.await.unwrap(),
        ping.await.unwrap(),
    );
    // Nothing failed, and each result is whole (no interleaving).
    assert_eq!(slow.ok().rows_returned, 1);
    assert_eq!(slow.columns().len(), 1);
    assert_eq!(quick.rows(), vec![&vec![Value::Text("quick".into())]]);
    assert!(ping.is_ok());
    // The later ones waited for the first.
    assert!(started.elapsed() >= Duration::from_millis(500));
}

/// Cancel racing with the end of a query must never leak into the next one.
#[tokio::test]
#[ignore = "needs docker compose"]
async fn cancel_never_leaks_into_the_next_query() {
    use rowlark_core::Driver;
    use std::sync::Arc;
    use std::time::Duration;

    let driver = Arc::new(pg16().await);
    for round in 0..30u64 {
        let background = Arc::clone(&driver);
        let query = tokio::spawn(async move { run(&background, "SELECT pg_sleep(5)").await });
        // A different delay each round, from "not started yet" to "well under way".
        tokio::time::sleep(Duration::from_millis(round % 7 * 5)).await;
        driver.cancel().await.expect("cancel");
        // Twice: a second cancel must be harmless too.
        driver.cancel().await.expect("second cancel");

        let r = query.await.expect("task");
        assert!(
            matches!(r.result, Err(DriverError::Cancelled)) || r.result.is_ok(),
            "round {round}: {:?}",
            r.result
        );
        // Whatever happened, the very next query is a normal one.
        let next = run(&driver, "SELECT 'fine'").await;
        assert_eq!(
            next.scalar(),
            &Value::Text("fine".into()),
            "round {round}: {:?}",
            next.result
        );
    }
}
