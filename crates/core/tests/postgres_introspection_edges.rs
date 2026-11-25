//! Introspection with nasty names and unusual table shapes, and with a user
//! who has only some privileges.
//!
//! Needs `docker compose up -d`; run with:
//!
//!     cargo test -p rowlark-core --test postgres_introspection_edges -- --ignored
//!
//! Every test uses its own schema (and role or database where needed), so the
//! tests can run side by side.

mod common;

use common::{PG16, config, exec, pg16, run};
use rowlark_core::{Driver, DriverError, PostgresDriver, TableKind};

/// Quote a name the way SQL wants it: wrap in double quotes, double any inside.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn names(items: Vec<rowlark_core::TableInfo>) -> Vec<String> {
    items.into_iter().map(|t| t.name).collect()
}

// ------------------------------------------------------------ nasty names --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn names_with_quotes_spaces_unicode_and_keywords_survive_every_call() {
    let d = pg16().await;
    let schema = "Edge \"Odd\" it's ünï";
    let table = "select \"x\" 'y' 表";
    // 63 bytes is the longest name Postgres keeps.
    let long = "t".repeat(63);
    exec(
        &d,
        &format!(
            "DROP SCHEMA IF EXISTS {s} CASCADE; CREATE SCHEMA {s}; \
             CREATE TABLE {s}.{t} (\"select\" int, \"with space\" text, \"üñí\" int, \
                 \"a\"\"b\" int, \"it's\" int); \
             CREATE TABLE {s}.{l} (id int)",
            s = ident(schema),
            t = ident(table),
            l = ident(&long),
        ),
    )
    .await;

    assert!(
        d.list_schemas()
            .await
            .unwrap()
            .contains(&schema.to_string())
    );

    let tables = names(d.list_tables(schema).await.unwrap());
    assert!(tables.contains(&table.to_string()), "{tables:?}");
    assert!(tables.contains(&long), "{tables:?}");

    let details = d.describe_table(schema, table).await.unwrap();
    let columns: Vec<&str> = details.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        columns,
        ["select", "with space", "üñí", "a\"b", "it's"],
        "column names must come back exactly as stored"
    );
    assert_eq!(
        d.describe_table(schema, &long).await.unwrap().columns.len(),
        1
    );

    exec(&d, &format!("DROP SCHEMA {} CASCADE", ident(schema))).await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_name_with_a_dot_is_one_name_not_schema_dot_table() {
    // `"a.b"` is a single table called a.b, not table b in schema a.
    let d = pg16().await;
    exec(
        &d,
        "DROP SCHEMA IF EXISTS edge_dot CASCADE; CREATE SCHEMA edge_dot; \
         CREATE TABLE edge_dot.\"a.b\" (x int)",
    )
    .await;
    let details = d.describe_table("edge_dot", "a.b").await.unwrap();
    assert_eq!(details.columns[0].name, "x");
    exec(&d, "DROP SCHEMA edge_dot CASCADE").await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn unknown_and_empty_schemas_and_tables_behave() {
    let d = pg16().await;
    exec(
        &d,
        "DROP SCHEMA IF EXISTS edge_empty CASCADE; CREATE SCHEMA edge_empty",
    )
    .await;
    assert!(d.list_tables("edge_empty").await.unwrap().is_empty());
    // A schema that does not exist lists nothing; it is not an error.
    assert!(
        d.list_tables("edge_no_such_schema")
            .await
            .unwrap()
            .is_empty()
    );
    // A table that does not exist gives the server's own answer.
    match d.describe_table("edge_empty", "nope").await {
        Err(DriverError::Server { code, .. }) => assert_eq!(code.as_deref(), Some("42P01")),
        other => panic!("expected 42P01, got {other:?}"),
    }
    exec(&d, "DROP SCHEMA edge_empty CASCADE").await;
}

// ------------------------------------------------------------ table shapes --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_table_with_no_columns_and_one_with_dropped_columns() {
    let d = pg16().await;
    exec(
        &d,
        "DROP SCHEMA IF EXISTS edge_shapes CASCADE; CREATE SCHEMA edge_shapes; \
         CREATE TABLE edge_shapes.nothing (); \
         CREATE TABLE edge_shapes.gappy (a int, junk text, b int, c int, \
             PRIMARY KEY (c, a)); \
         ALTER TABLE edge_shapes.gappy DROP COLUMN junk",
    )
    .await;

    let nothing = d.describe_table("edge_shapes", "nothing").await.unwrap();
    assert!(nothing.columns.is_empty());
    assert!(nothing.primary_key.is_empty());

    let gappy = d.describe_table("edge_shapes", "gappy").await.unwrap();
    let columns: Vec<&str> = gappy.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        columns,
        ["a", "b", "c"],
        "the dropped column must not appear"
    );
    // The key is reported in key order (c, a), not table order (a, c).
    assert_eq!(gappy.primary_key, ["c", "a"]);

    exec(&d, "DROP SCHEMA edge_shapes CASCADE").await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn partitioned_tables_list_the_parent_and_every_partition() {
    let d = pg16().await;
    exec(
        &d,
        "DROP SCHEMA IF EXISTS edge_part CASCADE; CREATE SCHEMA edge_part; \
         CREATE TABLE edge_part.m (id int, day date) PARTITION BY RANGE (day); \
         CREATE TABLE edge_part.m_2025 PARTITION OF edge_part.m \
             FOR VALUES FROM ('2025-01-01') TO ('2026-01-01'); \
         CREATE TABLE edge_part.m_2026 PARTITION OF edge_part.m \
             FOR VALUES FROM ('2026-01-01') TO ('2027-01-01'); \
         CREATE INDEX ON edge_part.m (day)",
    )
    .await;

    let tables = d.list_tables("edge_part").await.unwrap();
    assert_eq!(names(tables.clone()), ["m", "m_2025", "m_2026"]);
    assert!(tables.iter().all(|t| t.kind == TableKind::Table));

    // The parent and a partition both describe fine, and the partition got
    // its own copy of the parent's index.
    let parent = d.describe_table("edge_part", "m").await.unwrap();
    assert_eq!(parent.columns.len(), 2);
    assert_eq!(parent.indexes.len(), 1);
    let child = d.describe_table("edge_part", "m_2025").await.unwrap();
    assert_eq!(child.indexes.len(), 1);
    assert_eq!(child.indexes[0].columns, ["day"]);

    exec(&d, "DROP SCHEMA edge_part CASCADE").await;
}

// ----------------------------------------------------------------- indexes --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn expression_indexes_report_only_real_columns_and_keep_their_definition() {
    let d = pg16().await;
    exec(
        &d,
        "DROP SCHEMA IF EXISTS edge_expr CASCADE; CREATE SCHEMA edge_expr; \
         CREATE TABLE edge_expr.u (id int, email text); \
         CREATE INDEX only_expr ON edge_expr.u (lower(email)); \
         CREATE INDEX mixed ON edge_expr.u (id, lower(email))",
    )
    .await;
    let details = d.describe_table("edge_expr", "u").await.unwrap();
    let find = |name: &str| details.indexes.iter().find(|i| i.name == name).unwrap();

    // The expression is not a column, so it is not listed as one...
    assert!(find("only_expr").columns.is_empty());
    assert_eq!(find("mixed").columns, ["id"]);
    // ...but the full definition is there for the user to read.
    assert!(find("only_expr").definition.contains("lower(email"));
    assert!(find("mixed").definition.contains("lower(email"));

    exec(&d, "DROP SCHEMA edge_expr CASCADE").await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn include_columns_are_not_reported_as_part_of_the_key() {
    // `UNIQUE (a) INCLUDE (b)` makes `a` unique, not the pair (a, b). Listing
    // [a, b] as the columns would tell the user the wrong thing.
    let d = pg16().await;
    exec(
        &d,
        "DROP SCHEMA IF EXISTS edge_include CASCADE; CREATE SCHEMA edge_include; \
         CREATE TABLE edge_include.t (a int, b int); \
         CREATE UNIQUE INDEX cover ON edge_include.t (a) INCLUDE (b)",
    )
    .await;
    let details = d.describe_table("edge_include", "t").await.unwrap();
    let cover = details.indexes.iter().find(|i| i.name == "cover").unwrap();
    assert!(cover.unique);
    assert_eq!(cover.columns, ["a"]);
    assert!(cover.definition.contains("INCLUDE"));

    exec(&d, "DROP SCHEMA edge_include CASCADE").await;
}

// ------------------------------------------------------------ foreign keys --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn foreign_keys_across_schemas_and_to_the_same_table() {
    let d = pg16().await;
    exec(
        &d,
        "DROP SCHEMA IF EXISTS edge_fk_a CASCADE; DROP SCHEMA IF EXISTS \"Edge FK B\" CASCADE; \
         CREATE SCHEMA edge_fk_a; CREATE SCHEMA \"Edge FK B\"; \
         CREATE TABLE \"Edge FK B\".\"Parent T\" (\"Id\" int PRIMARY KEY); \
         CREATE TABLE edge_fk_a.child ( \
             id int PRIMARY KEY, \
             parent_id int REFERENCES \"Edge FK B\".\"Parent T\" (\"Id\") ON DELETE SET DEFAULT, \
             boss_id int REFERENCES edge_fk_a.child (id))",
    )
    .await;
    let details = d.describe_table("edge_fk_a", "child").await.unwrap();
    assert_eq!(details.foreign_keys.len(), 2);

    let other = details
        .foreign_keys
        .iter()
        .find(|f| f.columns == ["parent_id"])
        .expect("cross-schema key");
    assert_eq!(other.referenced_schema, "Edge FK B");
    assert_eq!(other.referenced_table, "Parent T");
    assert_eq!(other.referenced_columns, ["Id"]);
    assert_eq!(other.on_delete, "SET DEFAULT");

    let own = details
        .foreign_keys
        .iter()
        .find(|f| f.columns == ["boss_id"])
        .expect("self-reference");
    assert_eq!(own.referenced_schema, "edge_fk_a");
    assert_eq!(own.referenced_table, "child");

    exec(
        &d,
        "DROP SCHEMA edge_fk_a CASCADE; DROP SCHEMA \"Edge FK B\" CASCADE",
    )
    .await;
}

// -------------------------------------------------------- partial privileges --

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_user_with_partial_privileges_can_still_browse_the_catalog() {
    let admin = pg16().await;
    // Remove a leftover role from an earlier failed run. (A failing statement
    // is fine here, so use `run`, which does not panic.)
    run(&admin, "DROP OWNED BY edge_reader; DROP ROLE edge_reader").await;
    exec(
        &admin,
        "DROP SCHEMA IF EXISTS edge_priv CASCADE; CREATE SCHEMA edge_priv; \
         CREATE TABLE edge_priv.visible (id int); \
         CREATE TABLE edge_priv.secret (id int); \
         CREATE ROLE edge_reader LOGIN PASSWORD 'edge_pw'; \
         GRANT CONNECT ON DATABASE rowlark_test TO edge_reader; \
         GRANT USAGE ON SCHEMA edge_priv TO edge_reader; \
         GRANT SELECT ON edge_priv.visible TO edge_reader",
    )
    .await;

    let mut cfg = config(PG16);
    cfg.user = "edge_reader".into();
    let limited = PostgresDriver::connect(&cfg, "edge_pw")
        .await
        .expect("connect");

    // Browsing works, like `\dt` in psql: the catalog lists everything...
    assert!(
        limited
            .list_schemas()
            .await
            .unwrap()
            .contains(&"edge_priv".to_string())
    );
    assert_eq!(
        names(limited.list_tables("edge_priv").await.unwrap()),
        ["secret", "visible"]
    );
    assert_eq!(
        limited
            .describe_table("edge_priv", "secret")
            .await
            .unwrap()
            .columns
            .len(),
        1
    );
    // ...and the database, not Rowlark, decides what can be read.
    assert!(
        run(&limited, "SELECT * FROM edge_priv.visible")
            .await
            .result
            .is_ok()
    );
    match run(&limited, "SELECT * FROM edge_priv.secret").await.result {
        Err(DriverError::Server { code, .. }) => assert_eq!(code.as_deref(), Some("42501")),
        other => panic!("expected permission denied, got {other:?}"),
    }

    drop(limited);
    exec(
        &admin,
        "DROP SCHEMA edge_priv CASCADE; DROP OWNED BY edge_reader; DROP ROLE edge_reader",
    )
    .await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn databases_the_user_cannot_connect_to_are_not_listed() {
    let admin = pg16().await;
    run(&admin, "DROP DATABASE IF EXISTS edge_locked").await;
    run(&admin, "DROP OWNED BY edge_guest; DROP ROLE edge_guest").await;
    exec(
        &admin,
        "CREATE ROLE edge_guest LOGIN PASSWORD 'edge_pw'; \
         GRANT CONNECT ON DATABASE rowlark_test TO edge_guest",
    )
    .await;
    // CREATE DATABASE cannot share a run with other statements.
    exec(&admin, "CREATE DATABASE edge_locked").await;
    exec(&admin, "REVOKE CONNECT ON DATABASE edge_locked FROM PUBLIC").await;

    let mut cfg = config(PG16);
    cfg.user = "edge_guest".into();
    let guest = PostgresDriver::connect(&cfg, "edge_pw")
        .await
        .expect("connect");

    let guest_sees = guest.list_databases().await.unwrap();
    assert!(
        guest_sees.contains(&"rowlark_test".to_string()),
        "{guest_sees:?}"
    );
    assert!(
        !guest_sees.contains(&"edge_locked".to_string()),
        "listed a database this user cannot connect to: {guest_sees:?}"
    );
    // The administrator can still see it.
    assert!(
        admin
            .list_databases()
            .await
            .unwrap()
            .contains(&"edge_locked".to_string())
    );

    drop(guest);
    exec(&admin, "DROP DATABASE edge_locked").await;
    exec(&admin, "DROP OWNED BY edge_guest; DROP ROLE edge_guest").await;
}
