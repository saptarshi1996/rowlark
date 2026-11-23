//! Column headers for queries that return no rows.
//!
//! The server only describes a result's columns together with its first row,
//! so an empty result has none. The driver asks the server to describe the
//! statement separately (on its introspection connection) to fill the gap.
//!
//! Needs `docker compose up -d`; run with:
//!
//!     cargo test -p rowlark-core --test postgres_empty_results -- --ignored

use rowlark_core::{
    Column, ConnectionConfig, Driver, PostgresDriver, QueryEvent, QuerySummary, Value,
};
use tokio::sync::mpsc;

struct Run {
    summary: QuerySummary,
    columns: Vec<Vec<Column>>,
    rows: Vec<Vec<Value>>,
}

async fn run(driver: &PostgresDriver, sql: &str) -> Run {
    let (tx, mut rx) = mpsc::channel(4);
    let collect = async {
        let (mut columns, mut rows) = (Vec::new(), Vec::new());
        while let Some(event) = rx.recv().await {
            match event {
                QueryEvent::Columns(c) => columns.push(c),
                QueryEvent::Rows(mut r) => rows.append(&mut r),
                QueryEvent::Notice(_) => {}
            }
        }
        (columns, rows)
    };
    let (summary, (columns, rows)) = tokio::join!(driver.run_query(sql, tx), collect);
    let summary = summary.unwrap_or_else(|e| panic!("{sql}\n  -> {e}"));
    Run {
        summary,
        columns,
        rows,
    }
}

async fn connect() -> PostgresDriver {
    let config = ConnectionConfig::new("127.0.0.1", 54316, "rowlark", "rowlark_test");
    PostgresDriver::connect(&config, "rowlark")
        .await
        .expect("is `docker compose up -d` running?")
}

fn names(columns: &[Column]) -> Vec<&str> {
    columns.iter().map(|c| c.name.as_str()).collect()
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn an_empty_select_still_reports_its_columns() {
    let driver = connect().await;
    run(
        &driver,
        "DROP SCHEMA IF EXISTS empty_test CASCADE; CREATE SCHEMA empty_test; \
         CREATE TABLE empty_test.users (id int PRIMARY KEY, email text, plan text)",
    )
    .await;

    let r = run(&driver, "SELECT * FROM empty_test.users").await;
    assert_eq!(r.summary.rows_returned, 0);
    assert!(r.rows.is_empty());
    assert_eq!(r.columns.len(), 1, "one result set");
    assert_eq!(names(&r.columns[0]), ["id", "email", "plan"]);
    assert_eq!(r.columns[0][0].type_name, "INT4");
    assert_eq!(r.columns[0][1].type_name, "TEXT");
    // The origin works here too, so even an empty result can be edited into.
    let origin = r.columns[0][1].origin.as_ref().expect("origin");
    assert_eq!(
        (
            origin.schema.as_str(),
            origin.table.as_str(),
            origin.column.as_str()
        ),
        ("empty_test", "users", "email")
    );

    // A filter that matches nothing, and an expression with no table at all.
    let r = run(
        &driver,
        "SELECT email AS contact FROM empty_test.users WHERE id = -1",
    )
    .await;
    assert_eq!(names(&r.columns[0]), ["contact"]);
    let r = run(&driver, "SELECT 1 AS one, 'x'::text AS two WHERE false").await;
    assert_eq!(names(&r.columns[0]), ["one", "two"]);

    // A statement that changes nothing and has no result columns sends none.
    let r = run(
        &driver,
        "UPDATE empty_test.users SET plan = 'x' WHERE id = -1",
    )
    .await;
    assert!(r.columns.is_empty());
    assert_eq!(r.summary.rows_affected, 0);
    let r = run(&driver, "SET application_name = 'Rowlark'").await;
    assert!(r.columns.is_empty());

    // INSERT ... RETURNING that inserted nothing still shows what it would return.
    let r = run(
        &driver,
        "INSERT INTO empty_test.users SELECT * FROM empty_test.users RETURNING id, plan",
    )
    .await;
    assert_eq!(names(&r.columns[0]), ["id", "plan"]);

    // A result with rows is untouched: one Columns event, from the real rows.
    run(
        &driver,
        "INSERT INTO empty_test.users VALUES (1, 'a@x.io', 'free')",
    )
    .await;
    let r = run(&driver, "SELECT * FROM empty_test.users").await;
    assert_eq!(r.columns.len(), 1);
    assert_eq!(r.rows.len(), 1);

    run(&driver, "DROP SCHEMA empty_test CASCADE").await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn several_empty_statements_just_have_no_headers() {
    let driver = connect().await;
    // Describing two statements at once fails inside the server. That must be
    // invisible: the query itself succeeded, so it succeeds, without headers.
    let r = run(&driver, "SELECT 1 WHERE false; SELECT 2 WHERE false").await;
    assert_eq!(r.summary.rows_returned, 0);
    assert!(r.columns.is_empty());
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn looking_up_headers_never_breaks_the_users_open_transaction() {
    let driver = connect().await;
    run(&driver, "BEGIN").await;
    run(&driver, "CREATE TEMP TABLE in_tx (a int)").await;

    // Two statements: the describe fails. If it had run on the user's own
    // connection, Postgres would now consider their transaction aborted.
    run(&driver, "SELECT 1 WHERE false; SELECT 2 WHERE false").await;

    // The transaction is alive: more work succeeds and can be committed.
    run(&driver, "INSERT INTO in_tx VALUES (42)").await;
    let r = run(&driver, "SELECT a FROM in_tx").await;
    assert_eq!(r.rows, vec![vec![Value::Int(42)]]);
    run(&driver, "COMMIT").await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn tables_only_this_session_can_see_have_no_headers() {
    let driver = connect().await;
    run(&driver, "CREATE TEMP TABLE only_mine (a int)").await;
    // The introspection connection cannot see this session's temp table, so it
    // cannot describe the query. Known limitation: no headers, but no error.
    let r = run(&driver, "SELECT * FROM only_mine").await;
    assert_eq!(r.summary.rows_returned, 0);
    assert!(r.columns.is_empty());
}
