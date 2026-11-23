//! Introspection tests for the Postgres driver: build a small real schema,
//! then check that what the driver reports matches what we created.
//!
//! Needs `docker compose up -d`; run with:
//!
//!     cargo test -p rowlark-core --test postgres_introspection -- --ignored
//!
//! The tables live in a normal schema (not TEMP tables) because introspection
//! uses its own connection, which cannot see another connection's temp tables.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rowlark_core::{
    ConnectionConfig, Driver, DriverError, ForeignKeyInfo, PostgresDriver, QueryEvent, TableKind,
};
use tokio::sync::mpsc;

const SCHEMA: &str = "introspect_test";

/// Run SQL and throw away the output (we only need it to have happened).
async fn exec(driver: &PostgresDriver, sql: &str) {
    let (tx, mut rx) = mpsc::channel::<QueryEvent>(4);
    let drain = async { while rx.recv().await.is_some() {} };
    let (result, ()) = tokio::join!(driver.run_query(sql, tx), drain);
    result.unwrap_or_else(|e| panic!("{sql}\n  -> {e}"));
}

/// Postgres 14..17 -> ports 54314..54317 (see docker-compose.yml).
const PORTS: [u16; 4] = [54314, 54315, 54316, 54317];

async fn connect(port: u16) -> PostgresDriver {
    let config = ConnectionConfig::new("127.0.0.1", port, "rowlark", "rowlark_test");
    PostgresDriver::connect(&config, "rowlark")
        .await
        .expect("is `docker compose up -d` running?")
}

/// Create the test schema from scratch (dropping any leftover from a failed run).
async fn build_schema(driver: &PostgresDriver) {
    exec(
        driver,
        &format!(
            "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; \
             CREATE SCHEMA {SCHEMA}; \
             CREATE TABLE {SCHEMA}.users ( \
                 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
                 email text NOT NULL UNIQUE, \
                 name text, \
                 plan text NOT NULL DEFAULT 'free', \
                 balance numeric(12,2) NOT NULL DEFAULT 0, \
                 created_at timestamptz DEFAULT now(), \
                 name_upper text GENERATED ALWAYS AS (upper(name)) STORED \
             ); \
             CREATE TABLE {SCHEMA}.orders ( \
                 id bigserial PRIMARY KEY, \
                 user_id bigint NOT NULL REFERENCES {SCHEMA}.users(id) ON DELETE CASCADE, \
                 total numeric(12,2), \
                 shipped_at timestamptz \
             ); \
             CREATE INDEX orders_user_idx ON {SCHEMA}.orders (user_id, shipped_at) \
                 WHERE shipped_at IS NOT NULL; \
             CREATE VIEW {SCHEMA}.big_orders AS \
                 SELECT id, total FROM {SCHEMA}.orders WHERE total > 100; \
             CREATE TABLE {SCHEMA}.composite (a int, b int, PRIMARY KEY (a, b)); \
             CREATE TABLE {SCHEMA}.child ( \
                 a int, b int, \
                 FOREIGN KEY (a, b) REFERENCES {SCHEMA}.composite (a, b) \
                     ON UPDATE CASCADE ON DELETE SET NULL \
             ); \
             CREATE TABLE {SCHEMA}.\"Odd Name\" (a int, \"B\" text);"
        ),
    )
    .await;
}

fn names<T>(items: &[T], name: impl Fn(&T) -> &str) -> Vec<String> {
    let mut names: Vec<String> = items.iter().map(|i| name(i).to_string()).collect();
    names.sort();
    names
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn introspection_describes_a_real_schema_on_every_postgres_version() {
    for port in PORTS {
        describe_real_schema(port).await;
    }
}

async fn describe_real_schema(port: u16) {
    let driver = connect(port).await;
    build_schema(&driver).await;

    // ---- databases and schemas ----
    let databases = driver.list_databases().await.expect("databases");
    assert!(
        databases.contains(&"rowlark_test".to_string()),
        "{databases:?}"
    );
    assert!(databases.contains(&"postgres".to_string()));
    assert!(
        !databases.contains(&"template0".to_string()),
        "templates are hidden"
    );

    let schemas = driver.list_schemas().await.expect("schemas");
    assert!(schemas.contains(&"public".to_string()));
    assert!(schemas.contains(&SCHEMA.to_string()));
    assert!(!schemas.contains(&"pg_catalog".to_string()));
    assert!(!schemas.contains(&"information_schema".to_string()));

    // ---- tables and views (not indexes or sequences) ----
    let tables = driver.list_tables(SCHEMA).await.expect("tables");
    assert_eq!(
        names(&tables, |t| &t.name),
        [
            "Odd Name",
            "big_orders",
            "child",
            "composite",
            "orders",
            "users"
        ]
    );
    let kind_of = |name: &str| tables.iter().find(|t| t.name == name).map(|t| t.kind);
    assert_eq!(kind_of("users"), Some(TableKind::Table));
    assert_eq!(kind_of("big_orders"), Some(TableKind::View));

    // ---- users: columns, identity, defaults, primary key, unique index ----
    let users = driver.describe_table(SCHEMA, "users").await.expect("users");
    let column = |name: &str| {
        users
            .columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no column {name}"))
    };
    assert_eq!(
        users
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        [
            "id",
            "email",
            "name",
            "plan",
            "balance",
            "created_at",
            "name_upper"
        ],
        "columns come back in table order"
    );
    assert_eq!(column("id").data_type, "bigint");
    assert!(column("id").identity);
    assert!(!column("id").nullable);
    assert!(!column("email").nullable);
    assert!(column("name").nullable);
    assert!(!column("email").identity);
    assert_eq!(column("plan").default.as_deref(), Some("'free'::text"));
    assert_eq!(column("balance").data_type, "numeric(12,2)");
    assert_eq!(column("balance").default.as_deref(), Some("0"));
    assert_eq!(column("created_at").data_type, "timestamp with time zone");
    assert_eq!(column("created_at").default.as_deref(), Some("now()"));
    assert!(column("name_upper").generated);
    assert!(!column("name").generated);

    assert_eq!(users.primary_key, ["id"]);
    let pkey = users
        .indexes
        .iter()
        .find(|i| i.primary)
        .expect("primary index");
    assert_eq!(pkey.name, "users_pkey");
    assert!(pkey.unique);
    assert_eq!(pkey.method, "btree");
    let email_index = users
        .indexes
        .iter()
        .find(|i| i.name == "users_email_key")
        .expect("email");
    assert!(email_index.unique && !email_index.primary);
    assert_eq!(email_index.columns, ["email"]);
    assert!(users.foreign_keys.is_empty());

    // ---- orders: serial default, multi-column partial index, foreign key ----
    let orders = driver
        .describe_table(SCHEMA, "orders")
        .await
        .expect("orders");
    let id_default = orders.columns[0].default.as_deref().unwrap_or("");
    assert!(id_default.starts_with("nextval("), "{id_default}");
    let partial = orders
        .indexes
        .iter()
        .find(|i| i.name == "orders_user_idx")
        .expect("partial index");
    assert_eq!(partial.columns, ["user_id", "shipped_at"], "order is kept");
    assert!(!partial.unique);
    assert!(
        partial.definition.contains("WHERE"),
        "{}",
        partial.definition
    );
    assert_eq!(
        orders.foreign_keys,
        vec![ForeignKeyInfo {
            name: "orders_user_id_fkey".into(),
            columns: vec!["user_id".into()],
            referenced_schema: SCHEMA.into(),
            referenced_table: "users".into(),
            referenced_columns: vec!["id".into()],
            on_update: "NO ACTION".into(),
            on_delete: "CASCADE".into(),
        }]
    );

    // ---- composite keys keep their column order ----
    let composite = driver
        .describe_table(SCHEMA, "composite")
        .await
        .expect("composite");
    assert_eq!(composite.primary_key, ["a", "b"]);
    let child = driver.describe_table(SCHEMA, "child").await.expect("child");
    assert_eq!(child.foreign_keys.len(), 1);
    let fk = &child.foreign_keys[0];
    assert_eq!(fk.columns, ["a", "b"]);
    assert_eq!(fk.referenced_columns, ["a", "b"]);
    assert_eq!(fk.on_update, "CASCADE");
    assert_eq!(fk.on_delete, "SET NULL");
    assert!(
        child.primary_key.is_empty(),
        "no primary key gives an empty list"
    );

    // ---- views have columns but no keys or indexes ----
    let view = driver
        .describe_table(SCHEMA, "big_orders")
        .await
        .expect("view");
    assert_eq!(
        view.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "total"]
    );
    assert!(view.indexes.is_empty() && view.primary_key.is_empty());

    // ---- names that need quoting work ----
    let odd = driver
        .describe_table(SCHEMA, "Odd Name")
        .await
        .expect("odd name");
    assert_eq!(
        odd.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["a", "B"]
    );

    // ---- a missing table gives the server's own error ----
    match driver.describe_table(SCHEMA, "nope").await {
        Err(DriverError::Server { code, message }) => {
            assert_eq!(code.as_deref(), Some("42P01"));
            assert!(message.contains("nope"), "{message}");
        }
        other => panic!("expected a server error, got {other:?}"),
    }
    // ... and the introspection connection still works afterwards.
    assert!(driver.list_tables(SCHEMA).await.is_ok());

    exec(&driver, &format!("DROP SCHEMA {SCHEMA} CASCADE")).await;
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn introspection_works_while_a_query_is_running() {
    let driver = Arc::new(connect(54316).await);
    let background = Arc::clone(&driver);
    // Keep the main connection busy for 3 seconds.
    let slow = tokio::spawn(async move { exec(&background, "SELECT pg_sleep(3)").await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let started = Instant::now();
    let schemas = driver.list_schemas().await.expect("schemas while busy");
    assert!(schemas.contains(&"public".to_string()));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "introspection waited for the running query: {:?}",
        started.elapsed()
    );
    slow.await.expect("slow query task");
}
