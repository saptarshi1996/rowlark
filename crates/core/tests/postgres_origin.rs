//! Tests for "where did this result column come from?" (`Column::origin`).
//!
//! Needs `docker compose up -d`; run with:
//!
//!     cargo test -p rowlark-core --test postgres_origin -- --ignored

use rowlark_core::{Column, ColumnOrigin, ConnectionConfig, Driver, PostgresDriver, QueryEvent};
use tokio::sync::mpsc;

const SCHEMA: &str = "origin_test";

/// Run SQL and return the columns of its (last) result set.
async fn columns_of(driver: &PostgresDriver, sql: &str) -> Vec<Column> {
    let (tx, mut rx) = mpsc::channel(4);
    let collect = async {
        let mut columns = Vec::new();
        while let Some(event) = rx.recv().await {
            if let QueryEvent::Columns(c) = event {
                columns = c;
            }
        }
        columns
    };
    let (result, columns) = tokio::join!(driver.run_query(sql, tx), collect);
    result.unwrap_or_else(|e| panic!("{sql}\n  -> {e}"));
    columns
}

fn origin(schema: &str, table: &str, column: &str) -> Option<ColumnOrigin> {
    Some(ColumnOrigin {
        schema: schema.into(),
        table: table.into(),
        column: column.into(),
    })
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn columns_are_traced_back_to_their_tables() {
    let config = ConnectionConfig::new("127.0.0.1", 54316, "rowlark", "rowlark_test");
    let driver = PostgresDriver::connect(&config, "rowlark")
        .await
        .expect("connect");

    columns_of(
        &driver,
        &format!(
            "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; \
             CREATE SCHEMA {SCHEMA}; \
             CREATE TABLE {SCHEMA}.users (id int PRIMARY KEY, junk text, email text); \
             ALTER TABLE {SCHEMA}.users DROP COLUMN junk; \
             CREATE TABLE {SCHEMA}.orders (id int PRIMARY KEY, user_id int, total numeric); \
             CREATE TABLE {SCHEMA}.\"Odd Table\" (\"Odd Col\" int); \
             CREATE VIEW {SCHEMA}.user_view AS SELECT id, email FROM {SCHEMA}.users; \
             INSERT INTO {SCHEMA}.users VALUES (1, 'a@x.io'); \
             INSERT INTO {SCHEMA}.orders VALUES (10, 1, 5.5); \
             INSERT INTO {SCHEMA}.\"Odd Table\" VALUES (7)"
        ),
    )
    .await;

    // A plain table column, an alias, an expression and an aggregate.
    let cols = columns_of(
        &driver,
        &format!(
            "SELECT id, email AS contact, id + 1 AS next, count(*) OVER () AS n \
             FROM {SCHEMA}.users"
        ),
    )
    .await;
    assert_eq!(cols[0].origin, origin(SCHEMA, "users", "id"));
    // The alias is only the display name; the origin is the real column.
    assert_eq!(cols[1].name, "contact");
    assert_eq!(cols[1].origin, origin(SCHEMA, "users", "email"));
    // Computed values have no single source.
    assert_eq!(cols[2].origin, None);
    assert_eq!(cols[3].origin, None);

    // A join: each column points at its own table. (Dropped column numbers in
    // `users` must not shift the mapping.)
    let cols = columns_of(
        &driver,
        &format!(
            "SELECT u.email, o.id AS order_id, o.total \
             FROM {SCHEMA}.users u JOIN {SCHEMA}.orders o ON o.user_id = u.id"
        ),
    )
    .await;
    assert_eq!(cols[0].origin, origin(SCHEMA, "users", "email"));
    assert_eq!(cols[1].origin, origin(SCHEMA, "orders", "id"));
    assert_eq!(cols[2].origin, origin(SCHEMA, "orders", "total"));

    // Names that need quoting.
    let cols = columns_of(&driver, &format!("SELECT * FROM {SCHEMA}.\"Odd Table\"")).await;
    assert_eq!(cols[0].origin, origin(SCHEMA, "Odd Table", "Odd Col"));

    // A table that only exists in this session.
    columns_of(
        &driver,
        "CREATE TEMP TABLE scratch (a int, b int); INSERT INTO scratch VALUES (1, 2)",
    )
    .await;
    let cols = columns_of(&driver, "SELECT b FROM scratch").await;
    let temp = cols[0].origin.as_ref().expect("temp table origin");
    assert!(temp.schema.starts_with("pg_temp"), "{}", temp.schema);
    assert_eq!(
        (temp.table.as_str(), temp.column.as_str()),
        ("scratch", "b")
    );

    // A view: the server traces its columns to the underlying table, or to
    // nothing; either way it must never point at something wrong.
    let cols = columns_of(&driver, &format!("SELECT * FROM {SCHEMA}.user_view")).await;
    let view_origin = &cols[1].origin;
    assert!(
        *view_origin == origin(SCHEMA, "users", "email")
            || *view_origin == origin(SCHEMA, "user_view", "email")
            || view_origin.is_none(),
        "{view_origin:?}"
    );

    // Each statement of a multi-statement run reports its own columns.
    let cols = columns_of(
        &driver,
        &format!("SELECT id FROM {SCHEMA}.users; SELECT total FROM {SCHEMA}.orders"),
    )
    .await;
    assert_eq!(cols[0].origin, origin(SCHEMA, "orders", "total"));

    columns_of(&driver, &format!("DROP SCHEMA {SCHEMA} CASCADE")).await;
}
