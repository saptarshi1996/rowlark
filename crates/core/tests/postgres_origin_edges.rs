//! Column origins in awkward queries. The one rule: an origin is either exactly
//! right or absent. It must never point at the wrong table or column, because
//! the editor uses it to decide which row to UPDATE.
//!
//! Needs `docker compose up -d`; run with:
//!
//!     cargo test -p rowlark-core --test postgres_origin_edges -- --ignored

mod common;

use common::{exec, pg16, run};
use rowlark_core::{Column, ColumnOrigin, PostgresDriver};

/// Columns of the (last) result set of `sql`.
async fn columns_of(driver: &PostgresDriver, sql: &str) -> Vec<Column> {
    let r = exec(driver, sql).await;
    r.columns().last().expect("a result set").to_vec()
}

fn origin(schema: &str, table: &str, column: &str) -> Option<ColumnOrigin> {
    Some(ColumnOrigin {
        schema: schema.into(),
        table: table.into(),
        column: column.into(),
    })
}

/// Fresh schema with a `users` table (and a couple of rows).
async fn setup(driver: &PostgresDriver, schema: &str, schema_extra: &str) {
    exec(
        driver,
        &format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}; \
             CREATE TABLE {schema}.users (id int PRIMARY KEY, email text); \
             INSERT INTO {schema}.users VALUES (1, 'a@x.io'), (2, 'b@x.io'); \
             {schema_extra}"
        ),
    )
    .await;
}

async fn teardown(driver: &PostgresDriver, schema: &str) {
    exec(driver, &format!("DROP SCHEMA {schema} CASCADE")).await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_table_alias_does_not_change_the_real_table_name() {
    const SCHEMA: &str = "origin_edges_0";
    let d = pg16().await;
    setup(&d, SCHEMA, "").await;
    let cols = columns_of(
        &d,
        &format!("SELECT \"My Alias\".id, \"My Alias\".email FROM {SCHEMA}.users AS \"My Alias\""),
    )
    .await;
    assert_eq!(cols[0].origin, origin(SCHEMA, "users", "id"));
    assert_eq!(cols[1].origin, origin(SCHEMA, "users", "email"));
    teardown(&d, SCHEMA).await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn the_same_column_twice_and_duplicate_names() {
    const SCHEMA: &str = "origin_edges_1";
    let d = pg16().await;
    setup(&d, SCHEMA, "").await;
    let cols = columns_of(
        &d,
        &format!("SELECT id, id, id AS again FROM {SCHEMA}.users"),
    )
    .await;
    assert_eq!(cols[0].origin, origin(SCHEMA, "users", "id"));
    assert_eq!(cols[1].origin, origin(SCHEMA, "users", "id"));
    assert_eq!(cols[2].origin, origin(SCHEMA, "users", "id"));
    assert_eq!(cols[2].name, "again");

    // Two result columns with the same name stay two columns, in order.
    let cols = columns_of(&d, "SELECT 1 AS a, 2 AS a").await;
    assert_eq!(cols.len(), 2);
    assert_eq!((cols[0].name.as_str(), cols[1].name.as_str()), ("a", "a"));
    assert_eq!(cols[0].origin, None);
    teardown(&d, SCHEMA).await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_self_join_and_an_outer_join_keep_their_origins() {
    const SCHEMA: &str = "origin_edges_2";
    let d = pg16().await;
    setup(
        &d,
        SCHEMA,
        &format!("CREATE TABLE {SCHEMA}.notes (id int, user_id int, body text);"),
    )
    .await;
    let cols = columns_of(
        &d,
        &format!(
            "SELECT a.id, b.email, n.body FROM {SCHEMA}.users a \
             JOIN {SCHEMA}.users b ON b.id = a.id \
             LEFT JOIN {SCHEMA}.notes n ON n.user_id = a.id"
        ),
    )
    .await;
    assert_eq!(cols[0].origin, origin(SCHEMA, "users", "id"));
    assert_eq!(cols[1].origin, origin(SCHEMA, "users", "email"));
    // The nullable side of an outer join still comes from its table.
    assert_eq!(cols[2].origin, origin(SCHEMA, "notes", "body"));
    teardown(&d, SCHEMA).await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn set_operations_and_expressions_have_no_origin() {
    const SCHEMA: &str = "origin_edges_3";
    let d = pg16().await;
    setup(&d, SCHEMA, "").await;
    let cols = columns_of(
        &d,
        &format!(
            "SELECT id, email FROM {SCHEMA}.users UNION ALL \
             SELECT id, email FROM {SCHEMA}.users"
        ),
    )
    .await;
    // A UNION column has two possible sources, so it must not claim one.
    assert_eq!(cols[0].origin, None);
    assert_eq!(cols[1].origin, None);

    let cols = columns_of(
        &d,
        &format!("SELECT coalesce(email, 'none'), email || '!', id::text FROM {SCHEMA}.users"),
    )
    .await;
    assert!(cols.iter().all(|c| c.origin.is_none()), "{cols:?}");
    teardown(&d, SCHEMA).await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn ctes_and_subqueries_are_exactly_right_or_absent() {
    const SCHEMA: &str = "origin_edges_4";
    let d = pg16().await;
    setup(&d, SCHEMA, "").await;
    for sql in [
        format!("WITH x AS (SELECT id, email FROM {SCHEMA}.users) SELECT email FROM x"),
        format!("SELECT t.email FROM (SELECT email FROM {SCHEMA}.users) t"),
        format!("SELECT email FROM {SCHEMA}.users WHERE id IN (SELECT id FROM {SCHEMA}.users)"),
    ] {
        let cols = columns_of(&d, &sql).await;
        assert!(
            cols[0].origin == origin(SCHEMA, "users", "email") || cols[0].origin.is_none(),
            "{sql}\n  -> {:?}",
            cols[0].origin
        );
    }
    teardown(&d, SCHEMA).await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn returning_reports_the_written_tables_columns() {
    const SCHEMA: &str = "origin_edges_7";
    // Editing relies on this: an INSERT/UPDATE ... RETURNING row can be traced
    // back to its table.
    let d = pg16().await;
    setup(&d, SCHEMA, "").await;
    for sql in [
        format!("INSERT INTO {SCHEMA}.users VALUES (3, 'c@x.io') RETURNING id, email"),
        format!("UPDATE {SCHEMA}.users SET email = 'z' WHERE id = 3 RETURNING id, email"),
        format!("DELETE FROM {SCHEMA}.users WHERE id = 3 RETURNING id, email"),
    ] {
        let cols = columns_of(&d, &sql).await;
        assert_eq!(cols[0].origin, origin(SCHEMA, "users", "id"), "{sql}");
        assert_eq!(cols[1].origin, origin(SCHEMA, "users", "email"), "{sql}");
    }
    teardown(&d, SCHEMA).await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn partitioned_tables_trace_to_the_table_that_was_queried() {
    const SCHEMA: &str = "origin_edges_5";
    let d = pg16().await;
    setup(
        &d,
        SCHEMA,
        &format!(
            "CREATE TABLE {SCHEMA}.m (id int, day date) PARTITION BY RANGE (day); \
             CREATE TABLE {SCHEMA}.m_2026 PARTITION OF {SCHEMA}.m \
                 FOR VALUES FROM ('2026-01-01') TO ('2027-01-01'); \
             INSERT INTO {SCHEMA}.m VALUES (1, '2026-06-01');"
        ),
    )
    .await;
    let parent = columns_of(&d, &format!("SELECT id FROM {SCHEMA}.m")).await;
    assert_eq!(parent[0].origin, origin(SCHEMA, "m", "id"));
    let child = columns_of(&d, &format!("SELECT id FROM {SCHEMA}.m_2026")).await;
    assert_eq!(child[0].origin, origin(SCHEMA, "m_2026", "id"));
    teardown(&d, SCHEMA).await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn renames_between_runs_are_seen_immediately() {
    const SCHEMA: &str = "origin_edges_8";
    // No stale cache: after a rename, the next query reports the new names.
    let d = pg16().await;
    setup(&d, SCHEMA, "").await;
    let before = columns_of(&d, &format!("SELECT email FROM {SCHEMA}.users")).await;
    assert_eq!(before[0].origin, origin(SCHEMA, "users", "email"));

    exec(
        &d,
        &format!(
            "ALTER TABLE {SCHEMA}.users RENAME COLUMN email TO mail; \
             ALTER TABLE {SCHEMA}.users RENAME TO people"
        ),
    )
    .await;
    let after = columns_of(&d, &format!("SELECT mail FROM {SCHEMA}.people")).await;
    assert_eq!(after[0].origin, origin(SCHEMA, "people", "mail"));

    // A column added later is traced too (the table was seen before it existed).
    exec(
        &d,
        &format!("ALTER TABLE {SCHEMA}.people ADD COLUMN nick text"),
    )
    .await;
    let added = columns_of(&d, &format!("SELECT nick FROM {SCHEMA}.people")).await;
    assert_eq!(added[0].origin, origin(SCHEMA, "people", "nick"));

    // Moving the table to another schema keeps its id but changes its address.
    let moved = format!("{SCHEMA}_moved");
    exec(
        &d,
        &format!(
            "DROP SCHEMA IF EXISTS {moved} CASCADE; CREATE SCHEMA {moved}; \
             ALTER TABLE {SCHEMA}.people SET SCHEMA {moved}"
        ),
    )
    .await;
    let cols = columns_of(&d, &format!("SELECT mail FROM {moved}.people")).await;
    assert_eq!(cols[0].origin, origin(&moved, "people", "mail"));
    exec(&d, &format!("DROP SCHEMA {moved} CASCADE")).await;
    teardown(&d, SCHEMA).await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn an_empty_result_still_knows_where_its_columns_came_from() {
    const SCHEMA: &str = "origin_edges_6";
    let d = pg16().await;
    setup(&d, SCHEMA, "").await;
    let r = run(
        &d,
        &format!("SELECT id, email FROM {SCHEMA}.users WHERE false"),
    )
    .await;
    r.ok();
    assert!(r.rows().is_empty());
    let cols = r.columns()[0].clone();
    assert_eq!(cols[0].origin, origin(SCHEMA, "users", "id"));
    assert_eq!(cols[1].origin, origin(SCHEMA, "users", "email"));
    teardown(&d, SCHEMA).await;
}
