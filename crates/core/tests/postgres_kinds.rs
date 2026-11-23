//! Introspection of less common objects: materialized views, foreign and
//! partitioned tables, every foreign-key action, other index kinds, row
//! estimates and a large schema.
//!
//! Needs `docker compose up -d`; run with:
//!
//!     cargo test -p rowlark-core --test postgres_kinds -- --ignored

mod common;

use std::time::{Duration, Instant};

use common::*;
use rowlark_core::{Driver, DriverError, TableKind};

const SCHEMA: &str = "kinds_test";

#[tokio::test]
#[ignore = "needs docker compose"]
async fn every_kind_of_table_is_listed_with_the_right_kind() {
    for port in PG_PORTS {
        let driver = connect_to(port).await;
        exec(
            &driver,
            &format!(
                "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; \
                 CREATE SCHEMA {SCHEMA}; \
                 CREATE TABLE {SCHEMA}.plain (id int PRIMARY KEY, n int); \
                 CREATE VIEW {SCHEMA}.v AS SELECT id FROM {SCHEMA}.plain; \
                 CREATE MATERIALIZED VIEW {SCHEMA}.mv AS SELECT id, n FROM {SCHEMA}.plain; \
                 CREATE FOREIGN DATA WRAPPER kinds_fdw; \
                 CREATE SERVER kinds_srv FOREIGN DATA WRAPPER kinds_fdw; \
                 CREATE FOREIGN TABLE {SCHEMA}.ft (a int, b text) SERVER kinds_srv; \
                 CREATE TABLE {SCHEMA}.parted (id int, d date) PARTITION BY RANGE (d); \
                 CREATE TABLE {SCHEMA}.parted_2026 PARTITION OF {SCHEMA}.parted \
                     FOR VALUES FROM ('2026-01-01') TO ('2027-01-01'); \
                 CREATE SEQUENCE {SCHEMA}.seq; \
                 CREATE TYPE {SCHEMA}.mood AS ENUM ('a'); \
                 CREATE SCHEMA \"Odd Schema\"; \
                 CREATE TABLE \"Odd Schema\".t (x int)"
            ),
        )
        .await;

        let tables = driver.list_tables(SCHEMA).await.expect("tables");
        let kind = |name: &str| tables.iter().find(|t| t.name == name).map(|t| t.kind);
        assert_eq!(kind("plain"), Some(TableKind::Table), "port {port}");
        assert_eq!(kind("v"), Some(TableKind::View), "port {port}");
        assert_eq!(kind("mv"), Some(TableKind::MaterializedView), "port {port}");
        assert_eq!(kind("ft"), Some(TableKind::ForeignTable), "port {port}");
        // A partitioned table is a table, and so is each of its partitions.
        assert_eq!(kind("parted"), Some(TableKind::Table), "port {port}");
        assert_eq!(kind("parted_2026"), Some(TableKind::Table), "port {port}");
        // Sequences, types and indexes are not tables.
        assert_eq!(kind("seq"), None);
        assert_eq!(kind("mood"), None);
        assert_eq!(kind("plain_pkey"), None);

        // Every kind can be described.
        for name in ["plain", "v", "mv", "ft", "parted", "parted_2026"] {
            let details = driver
                .describe_table(SCHEMA, name)
                .await
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(!details.columns.is_empty(), "{name}");
        }
        let mv = driver.describe_table(SCHEMA, "mv").await.unwrap();
        assert_eq!(
            mv.columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["id", "n"]
        );
        let ft = driver.describe_table(SCHEMA, "ft").await.unwrap();
        assert_eq!(ft.columns[1].data_type, "text");
        // The partition's parent key shows up on the parent.
        let parted = driver.describe_table(SCHEMA, "parted").await.unwrap();
        assert_eq!(parted.columns.len(), 2);

        // A schema whose name needs quoting, and the schema list.
        assert!(
            driver
                .list_schemas()
                .await
                .unwrap()
                .contains(&"Odd Schema".to_string())
        );
        let odd = driver.list_tables("Odd Schema").await.unwrap();
        assert_eq!(odd.len(), 1);
        assert_eq!(
            driver
                .describe_table("Odd Schema", "t")
                .await
                .unwrap()
                .columns[0]
                .name,
            "x"
        );

        // A schema with nothing in it, and one that does not exist: empty, not an error.
        exec(&driver, &format!("CREATE SCHEMA {SCHEMA}_empty")).await;
        assert!(
            driver
                .list_tables(&format!("{SCHEMA}_empty"))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            driver
                .list_tables("no_such_schema")
                .await
                .unwrap()
                .is_empty()
        );

        exec(
            &driver,
            &format!(
                "DROP SCHEMA {SCHEMA} CASCADE; DROP SCHEMA {SCHEMA}_empty; \
                 DROP SCHEMA \"Odd Schema\" CASCADE; \
                 DROP SERVER kinds_srv CASCADE; DROP FOREIGN DATA WRAPPER kinds_fdw CASCADE"
            ),
        )
        .await;
    }
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn every_foreign_key_action_and_index_kind_is_reported() {
    for port in PG_PORTS {
        let driver = connect_to(port).await;
        exec(
            &driver,
            &format!(
                "DROP SCHEMA IF EXISTS {SCHEMA}_fk CASCADE; \
                 CREATE SCHEMA {SCHEMA}_fk; \
                 CREATE TABLE {SCHEMA}_fk.parent (id int PRIMARY KEY, code text UNIQUE); \
                 CREATE TABLE {SCHEMA}_fk.child ( \
                     a int DEFAULT 1 REFERENCES {SCHEMA}_fk.parent(id) ON DELETE RESTRICT ON UPDATE NO ACTION, \
                     b int DEFAULT 1 REFERENCES {SCHEMA}_fk.parent(id) ON DELETE SET DEFAULT ON UPDATE SET NULL, \
                     c int REFERENCES {SCHEMA}_fk.parent(id) ON DELETE NO ACTION ON UPDATE CASCADE, \
                     d text REFERENCES {SCHEMA}_fk.parent(code), \
                     body jsonb, tags text[], email text \
                 ); \
                 CREATE INDEX child_body_gin ON {SCHEMA}_fk.child USING gin (body); \
                 CREATE INDEX child_tags_gin ON {SCHEMA}_fk.child USING gin (tags); \
                 CREATE INDEX child_lower_email ON {SCHEMA}_fk.child (lower(email)); \
                 CREATE UNIQUE INDEX child_a_b ON {SCHEMA}_fk.child (a, b) WHERE a > 0"
            ),
        )
        .await;

        let child = driver
            .describe_table(&format!("{SCHEMA}_fk"), "child")
            .await
            .unwrap();
        let fk = |column: &str| {
            child
                .foreign_keys
                .iter()
                .find(|f| f.columns == [column])
                .unwrap_or_else(|| panic!("no foreign key on {column}"))
        };
        assert_eq!(
            (fk("a").on_delete.as_str(), fk("a").on_update.as_str()),
            ("RESTRICT", "NO ACTION")
        );
        assert_eq!(
            (fk("b").on_delete.as_str(), fk("b").on_update.as_str()),
            ("SET DEFAULT", "SET NULL")
        );
        assert_eq!(
            (fk("c").on_delete.as_str(), fk("c").on_update.as_str()),
            ("NO ACTION", "CASCADE")
        );
        // A foreign key to a unique column (not the primary key).
        assert_eq!(fk("d").referenced_columns, ["code"]);

        let index = |name: &str| {
            child
                .indexes
                .iter()
                .find(|i| i.name == name)
                .unwrap_or_else(|| panic!("no index {name}"))
        };
        assert_eq!(index("child_body_gin").method, "gin");
        assert_eq!(index("child_body_gin").columns, ["body"]);
        assert_eq!(index("child_tags_gin").method, "gin");
        // An expression index has no plain column, but its definition says it all.
        assert!(index("child_lower_email").columns.is_empty());
        assert!(
            index("child_lower_email")
                .definition
                .contains("lower(email)")
        );
        // A unique, partial, two-column index.
        let ab = index("child_a_b");
        assert!(ab.unique && !ab.primary);
        assert_eq!(ab.columns, ["a", "b"]);

        // Columns of unusual types keep the server's wording.
        let ty = |name: &str| {
            child
                .columns
                .iter()
                .find(|c| c.name == name)
                .unwrap()
                .data_type
                .as_str()
        };
        assert_eq!((ty("body"), ty("tags")), ("jsonb", "text[]"));

        exec(&driver, &format!("DROP SCHEMA {SCHEMA}_fk CASCADE")).await;
    }
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn row_estimates_appear_once_the_server_has_analyzed_the_table() {
    let driver = pg16().await;
    exec(
        &driver,
        &format!(
            "DROP SCHEMA IF EXISTS {SCHEMA}_est CASCADE; CREATE SCHEMA {SCHEMA}_est; \
             CREATE TABLE {SCHEMA}_est.t (id int); \
             INSERT INTO {SCHEMA}_est.t SELECT g FROM generate_series(1, 123) g"
        ),
    )
    .await;
    let schema = format!("{SCHEMA}_est");
    let find =
        |tables: Vec<rowlark_core::TableInfo>| tables.into_iter().find(|t| t.name == "t").unwrap();
    // Not analyzed yet: no estimate (Postgres 14+ says "unknown" with -1).
    assert_eq!(
        find(driver.list_tables(&schema).await.unwrap()).estimated_rows,
        None
    );
    exec(&driver, &format!("ANALYZE {schema}.t")).await;
    assert_eq!(
        find(driver.list_tables(&schema).await.unwrap()).estimated_rows,
        Some(123)
    );
    exec(&driver, &format!("DROP SCHEMA {schema} CASCADE")).await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_schema_with_fifteen_hundred_tables_lists_quickly() {
    let driver = pg16().await;
    let schema = format!("{SCHEMA}_big");
    let tables: String = (0..1500)
        .map(|i| format!("CREATE TABLE {schema}.t{i:04} (id int PRIMARY KEY, v text);"))
        .collect();
    exec(
        &driver,
        &format!("DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}; {tables}"),
    )
    .await;

    let started = Instant::now();
    let listed = driver.list_tables(&schema).await.expect("list");
    let elapsed = started.elapsed();
    assert_eq!(listed.len(), 1500);
    // Sorted by name, so the sidebar is stable.
    assert_eq!(listed[0].name, "t0000");
    assert_eq!(listed[1499].name, "t1499");
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
    // One table's details do not depend on how many others exist.
    let started = Instant::now();
    driver
        .describe_table(&schema, "t0750")
        .await
        .expect("describe");
    assert!(started.elapsed() < Duration::from_secs(2));

    exec(&driver, &format!("DROP SCHEMA {schema} CASCADE")).await;
    // Unrelated: a missing table still names itself in the server's error.
    match driver.describe_table("public", "nope_nope").await {
        Err(DriverError::Server { code, .. }) => assert_eq!(code.as_deref(), Some("42P01")),
        other => panic!("{other:?}"),
    }
}
