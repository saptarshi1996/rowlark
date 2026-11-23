//! Unusual data and the kinds of statements people actually type.
//!
//! Needs `docker compose up -d`; run with:
//!
//!     cargo test -p rowlark-core --test postgres_data -- --ignored

mod common;

use std::time::Duration;

use common::*;
use rowlark_core::{ConnectionConfig, DriverError, PostgresDriver, Value};
use tokio::time::timeout;

/// A driver connected to a database other than `rowlark_test`.
async fn connect_db(database: &str) -> PostgresDriver {
    let config = ConnectionConfig::new("127.0.0.1", PG16, "rowlark", database);
    PostgresDriver::connect(&config, "rowlark")
        .await
        .expect("connect")
}

/// Make a database with a given encoding, run `body` against it, then drop it.
async fn with_database<F, Fut>(name: &str, encoding: &str, body: F)
where
    F: FnOnce(PostgresDriver) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let admin = pg16().await;
    // CREATE/DROP DATABASE cannot share a transaction, so each goes alone.
    exec(
        &admin,
        &format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
    )
    .await;
    exec(
        &admin,
        &format!(
            "CREATE DATABASE {name} ENCODING '{encoding}' TEMPLATE template0 \
             LC_COLLATE 'C' LC_CTYPE 'C'"
        ),
    )
    .await;
    body(connect_db(name).await).await;
    exec(&admin, &format!("DROP DATABASE {name} WITH (FORCE)")).await;
}

// -------------------------------------------------------------- encodings --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_latin1_database_still_gives_us_proper_text() {
    with_database("enc_latin1", "LATIN1", |driver| async move {
        // The database stores Latin-1 bytes; the server converts to UTF-8 for us.
        exec(&driver, "CREATE TABLE t (s text)").await;
        exec(&driver, "INSERT INTO t VALUES ('café ñ ü £')").await;
        let r = exec(&driver, "SELECT s, octet_length(s) FROM t").await;
        assert_eq!(
            r.rows(),
            vec![&vec![Value::Text("café ñ ü £".into()), Value::Int(10)]]
        );
        assert_eq!(
            exec(&driver, "SHOW server_encoding").await.scalar(),
            &Value::Text("LATIN1".into())
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn text_that_is_not_valid_utf8_is_refused_or_exact_never_corrupted() {
    with_database("enc_ascii", "SQL_ASCII", |driver| async move {
        // SQL_ASCII databases keep raw bytes with no meaning attached, so 0xE9
        // (a Latin-1 "é") is not valid UTF-8.
        let sql = "SELECT E'\\351ab' AS raw, 'plain'::text AS ok";

        // By default we ask the server for UTF-8 and the server refuses to send
        // bytes that are not. We show its error rather than a "�" replacement.
        match run(&driver, sql).await.result {
            Err(DriverError::Server { code, message }) => {
                assert_eq!(code.as_deref(), Some("22021"));
                assert!(message.contains("invalid byte sequence"), "{message}");
            }
            other => panic!("expected the server to refuse, got {other:?}"),
        }

        // The user can ask for the raw bytes. We then pass them on exactly,
        // as `Bytes`, instead of mangling them into replacement characters.
        exec(&driver, "SET client_encoding = 'SQL_ASCII'").await;
        let r = exec(&driver, sql).await;
        assert_eq!(
            r.rows(),
            vec![&vec![
                Value::Bytes(vec![0xe9, b'a', b'b']),
                Value::Text("plain".into())
            ]]
        );
    })
    .await;
}

// ------------------------------------------------------------ big values --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_ten_megabyte_value_arrives_intact() {
    let driver = pg16().await;
    let r = exec(
        &driver,
        "SELECT repeat('0123456789', 1000000) AS big_text, \
                decode(repeat('ab', 5000000), 'hex') AS big_bytes, \
                ('{\"k\": \"' || repeat('v', 5000000) || '\"}')::json AS big_json",
    )
    .await;
    let row = r.rows()[0];
    assert!(
        matches!(&row[0], Value::Text(s) if s.len() == 10_000_000 && s.starts_with("0123456789"))
    );
    assert!(matches!(&row[1], Value::Bytes(b) if b.len() == 5_000_000 && b[0] == 0xab));
    assert!(matches!(&row[2], Value::Json(s) if s.len() == 5_000_009));
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_result_with_five_hundred_columns_works() {
    let driver = pg16().await;
    let select: Vec<String> = (0..500).map(|i| format!("{i} AS c{i}")).collect();
    let r = exec(&driver, &format!("SELECT {}", select.join(", "))).await;
    assert_eq!(r.columns()[0].len(), 500);
    assert_eq!(r.columns()[0][499].name, "c499");
    assert_eq!(r.rows()[0][499], Value::Int(499));
}

// ---------------------------------------------------- statements people type --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn everyday_statement_kinds_all_work() {
    let driver = pg16().await;
    exec(
        &driver,
        "DROP TABLE IF EXISTS kinds_t; CREATE TABLE kinds_t (id int PRIMARY KEY, v text)",
    )
    .await;

    // Writes that report what they did, with and without RETURNING.
    let r = exec(
        &driver,
        "INSERT INTO kinds_t VALUES (1, 'a'), (2, 'b') RETURNING id, v",
    )
    .await;
    assert_eq!(r.rows().len(), 2);
    let r = exec(&driver, "UPDATE kinds_t SET v = upper(v) WHERE id <= 2").await;
    assert_eq!(r.ok().rows_affected, 2);
    assert!(r.columns().is_empty());

    // WITH ... RETURNING, VALUES lists, set-returning functions.
    let r = exec(
        &driver,
        "WITH d AS (DELETE FROM kinds_t WHERE id = 2 RETURNING v) SELECT v FROM d",
    )
    .await;
    assert_eq!(r.scalar(), &Value::Text("B".into()));
    assert_eq!(
        exec(&driver, "VALUES (1, 'x'), (2, 'y')")
            .await
            .rows()
            .len(),
        2
    );
    assert_eq!(
        exec(&driver, "SELECT * FROM generate_series(1, 3)")
            .await
            .rows()
            .len(),
        3
    );

    // Transactions with savepoints: rolling back to one keeps the earlier work.
    exec(&driver, "BEGIN").await;
    exec(&driver, "INSERT INTO kinds_t VALUES (10, 'kept')").await;
    exec(&driver, "SAVEPOINT sp").await;
    exec(&driver, "INSERT INTO kinds_t VALUES (11, 'undone')").await;
    exec(&driver, "ROLLBACK TO SAVEPOINT sp").await;
    exec(&driver, "COMMIT").await;
    let r = exec(&driver, "SELECT id FROM kinds_t WHERE id >= 10 ORDER BY id").await;
    assert_eq!(r.rows(), vec![&vec![Value::Int(10)]]);

    // Statements that must run on their own (Postgres refuses them inside a
    // transaction block), which works because we send them as typed.
    exec(&driver, "VACUUM kinds_t").await;
    exec(
        &driver,
        "CREATE INDEX CONCURRENTLY kinds_t_v ON kinds_t (v)",
    )
    .await;
    exec(&driver, "ANALYZE kinds_t").await;

    // SQL-level prepared statements, EXPLAIN, SHOW, SET, DO.
    exec(
        &driver,
        "PREPARE q (int) AS SELECT v FROM kinds_t WHERE id = $1",
    )
    .await;
    assert_eq!(
        exec(&driver, "EXECUTE q(1)").await.scalar(),
        &Value::Text("A".into())
    );
    let plan = exec(&driver, "EXPLAIN SELECT * FROM kinds_t").await;
    assert!(matches!(plan.scalar(), Value::Text(t) if t.contains("kinds_t")));
    exec(&driver, "SET search_path = public, pg_catalog").await;
    assert!(matches!(
        exec(&driver, "SHOW search_path").await.scalar(),
        Value::Text(t) if t.contains("public")
    ));
    exec(&driver, "DO $$ BEGIN PERFORM 1; END $$").await;

    // Row locks and notifications.
    exec(&driver, "BEGIN; SELECT * FROM kinds_t FOR UPDATE; ROLLBACK").await;
    exec(
        &driver,
        "LISTEN kinds_channel; NOTIFY kinds_channel, 'hi'; UNLISTEN *",
    )
    .await;

    // A function that raises notices, and a procedure.
    exec(
        &driver,
        "CREATE OR REPLACE PROCEDURE kinds_proc() LANGUAGE plpgsql AS $$ \
         BEGIN RAISE NOTICE 'from procedure'; END $$",
    )
    .await;
    let r = exec(&driver, "CALL kinds_proc()").await;
    assert_eq!(r.notices().len(), 1);

    exec(&driver, "DROP PROCEDURE kinds_proc(); DROP TABLE kinds_t").await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn empty_and_comment_only_input_is_not_an_error() {
    let driver = pg16().await;
    for sql in ["", "   ", "-- just a comment", "/* block */", ";", ";;  ;"] {
        let r = timeout(Duration::from_secs(5), run(&driver, sql))
            .await
            .unwrap_or_else(|_| panic!("{sql:?} hung"));
        let summary = r.ok();
        assert_eq!(
            (summary.rows_returned, summary.rows_affected),
            (0, 0),
            "{sql:?}"
        );
        assert!(r.columns().is_empty() && r.rows().is_empty(), "{sql:?}");
    }
    // The connection is still good.
    assert_eq!(exec(&driver, "SELECT 1").await.scalar(), &Value::Int(1));
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn syntax_errors_carry_the_servers_code_and_message() {
    let driver = pg16().await;
    match run(&driver, "SELEC 1").await.result {
        Err(DriverError::Server { code, message }) => {
            assert_eq!(code.as_deref(), Some("42601"));
            assert!(message.contains("syntax error"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(exec(&driver, "SELECT 1").await.scalar(), &Value::Int(1));
}

/// COPY is a different wire protocol. We do not support it, but it must fail
/// cleanly, not hang or take the connection down.
#[tokio::test]
#[ignore = "needs docker compose"]
async fn copy_never_hangs_the_connection() {
    let driver = pg16().await;
    let r = timeout(
        Duration::from_secs(5),
        run(&driver, "COPY (SELECT 1) TO STDOUT"),
    )
    .await
    .expect("COPY hung the driver");
    // Either outcome is acceptable here; hanging or breaking the connection is not.
    let _ = r.result;
    let after = timeout(Duration::from_secs(5), run(&driver, "SELECT 'still alive'"))
        .await
        .expect("the connection hung after COPY");
    if after.result.is_err() {
        // The connection may need to be recreated; that must be reported as an error, not silence.
        assert!(matches!(
            after.result,
            Err(DriverError::Connection(_) | DriverError::Server { .. })
        ));
    }
}
