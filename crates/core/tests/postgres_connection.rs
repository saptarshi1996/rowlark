//! Connection-setting tests: timeout, application name, keepalive, URLs.
//!
//! Needs `docker compose up -d`; run with:
//!
//!     cargo test -p rowlark-core --test postgres_connection -- --ignored

use std::time::{Duration, Instant};

use rowlark_core::{ConnectionConfig, Driver, DriverError, PostgresDriver, QueryEvent, Value};
use tokio::sync::mpsc;

async fn scalar(driver: &PostgresDriver, sql: &str) -> Value {
    let (tx, mut rx) = mpsc::channel(4);
    let collect = async {
        let mut first = None;
        while let Some(event) = rx.recv().await {
            if let QueryEvent::Rows(rows) = event {
                first =
                    first.or_else(|| rows.into_iter().next().and_then(|r| r.into_iter().next()));
            }
        }
        first
    };
    let (result, value) = tokio::join!(driver.run_query(sql, tx), collect);
    result.unwrap_or_else(|e| panic!("{sql}: {e}"));
    value.expect("one value")
}

fn local() -> ConnectionConfig {
    ConnectionConfig::new("127.0.0.1", 54316, "rowlark", "rowlark_test")
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn the_application_name_is_visible_to_the_server() {
    let driver = PostgresDriver::connect(&local(), "rowlark")
        .await
        .expect("connect");
    assert_eq!(
        scalar(&driver, "SHOW application_name").await,
        Value::Text("Rowlark".into())
    );
    // DBAs can find us in pg_stat_activity.
    let others = scalar(
        &driver,
        "SELECT count(*) FROM pg_stat_activity WHERE application_name = 'Rowlark'",
    )
    .await;
    assert!(matches!(others, Value::Int(n) if n >= 1));

    let mut custom = local();
    custom.application_name = "Nightly report".into();
    let driver = PostgresDriver::connect(&custom, "rowlark")
        .await
        .expect("connect");
    assert_eq!(
        scalar(&driver, "SHOW application_name").await,
        Value::Text("Nightly report".into())
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn keepalive_is_requested_from_the_server_and_can_be_turned_off() {
    let driver = PostgresDriver::connect(&local(), "rowlark")
        .await
        .expect("connect");
    assert_eq!(
        scalar(&driver, "SHOW tcp_keepalives_idle").await,
        Value::Text("60".into())
    );
    assert_eq!(
        scalar(&driver, "SHOW tcp_keepalives_interval").await,
        Value::Text("10".into())
    );
    assert_eq!(
        scalar(&driver, "SHOW tcp_keepalives_count").await,
        Value::Text("3".into())
    );

    let mut off = local();
    off.keepalive = false;
    let driver = PostgresDriver::connect(&off, "rowlark")
        .await
        .expect("connect");
    assert_ne!(
        scalar(&driver, "SHOW tcp_keepalives_idle").await,
        Value::Text("60".into())
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn connecting_to_an_unreachable_host_gives_up_after_the_timeout() {
    // 10.255.255.1 is a private address nothing answers on, so the connection
    // attempt hangs (or is refused straight away on some networks).
    let mut config = ConnectionConfig::new("10.255.255.1", 5432, "rowlark", "rowlark_test");
    config.connect_timeout = Duration::from_secs(1);
    let started = Instant::now();
    let result = PostgresDriver::connect(&config, "rowlark").await;
    assert!(
        matches!(result, Err(DriverError::Connection(_))),
        "{:?}",
        result.err()
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_postgres_url_connects_end_to_end() {
    let (config, password) = ConnectionConfig::from_postgres_url(
        "postgres://rowlark:rowlark@127.0.0.1:54316/rowlark_test\
         ?sslmode=disable&application_name=From%20URL",
    )
    .expect("valid url");
    let driver = PostgresDriver::connect(&config, password.as_deref().unwrap_or(""))
        .await
        .expect("connect from url");
    assert_eq!(
        scalar(&driver, "SELECT current_database()::text").await,
        Value::Text("rowlark_test".into())
    );
    assert_eq!(
        scalar(&driver, "SHOW application_name").await,
        Value::Text("From URL".into())
    );
}
