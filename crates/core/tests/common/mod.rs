//! Helpers shared by the database tests. Each test file pulls this in with
//! `mod common;`. (`dead_code` is allowed because every test file uses only
//! some of these helpers.)
#![allow(dead_code)]

use rowlark_core::{
    Column, ConnectionConfig, Driver, DriverError, Notice, PostgresDriver, QueryEvent,
    QuerySummary, Value,
};
use tokio::sync::mpsc;

/// Postgres 14..17 test servers (see docker-compose.yml).
pub const PG_PORTS: [u16; 4] = [54314, 54315, 54316, 54317];
/// The one most tests use.
pub const PG16: u16 = 54316;
/// Postgres 16 with TLS switched on.
pub const PG16_TLS: u16 = 54326;

pub fn config(port: u16) -> ConnectionConfig {
    ConnectionConfig::new("127.0.0.1", port, "rowlark", "rowlark_test")
}

pub async fn connect_to(port: u16) -> PostgresDriver {
    PostgresDriver::connect(&config(port), "rowlark")
        .await
        .unwrap_or_else(|e| panic!("port {port}: is `docker compose up -d` running? {e}"))
}

pub async fn pg16() -> PostgresDriver {
    connect_to(PG16).await
}

/// Everything one `run_query` call produced.
pub struct Run {
    pub result: Result<QuerySummary, DriverError>,
    pub events: Vec<QueryEvent>,
}

impl Run {
    /// The summary, panicking with the server's message if the query failed.
    pub fn ok(&self) -> &QuerySummary {
        self.result
            .as_ref()
            .unwrap_or_else(|e| panic!("query failed: {e}"))
    }

    /// Every result set's columns, in order.
    pub fn columns(&self) -> Vec<&Vec<Column>> {
        self.events
            .iter()
            .filter_map(|e| match e {
                QueryEvent::Columns(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    /// All rows of all result sets, in order.
    pub fn rows(&self) -> Vec<&Vec<Value>> {
        self.events
            .iter()
            .filter_map(|e| match e {
                QueryEvent::Rows(rows) => Some(rows.iter()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    pub fn batch_sizes(&self) -> Vec<usize> {
        self.events
            .iter()
            .filter_map(|e| match e {
                QueryEvent::Rows(rows) => Some(rows.len()),
                _ => None,
            })
            .collect()
    }

    pub fn notices(&self) -> Vec<&Notice> {
        self.events
            .iter()
            .filter_map(|e| match e {
                QueryEvent::Notice(n) => Some(n),
                _ => None,
            })
            .collect()
    }

    /// The first value of the first row.
    pub fn scalar(&self) -> &Value {
        self.rows()
            .first()
            .and_then(|row| row.first())
            .expect("the query returned no rows")
    }
}

/// Run SQL and collect everything it sends. The query and the collector run
/// side by side (`join!`), because the channel holds only a few messages.
pub async fn run(driver: &PostgresDriver, sql: &str) -> Run {
    let (tx, mut rx) = mpsc::channel(4);
    let collect = async {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    };
    let (result, events) = tokio::join!(driver.run_query(sql, tx), collect);
    Run { result, events }
}

/// Run SQL that must succeed; return what it produced.
pub async fn exec(driver: &PostgresDriver, sql: &str) -> Run {
    let r = run(driver, sql).await;
    if let Err(e) = &r.result {
        panic!("{sql}\n  -> {e}");
    }
    r
}
