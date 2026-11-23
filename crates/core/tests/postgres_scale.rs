//! Big results: a million rows, and a consumer that cannot keep up.
//!
//! The time limits are generous so a slow CI machine does not cause a false
//! alarm; they exist to catch something being accidentally quadratic or
//! unbounded, not to benchmark. Real numbers are printed with `--nocapture`.
//!
//!     cargo test -p rowlark-core --test postgres_scale -- --ignored --nocapture

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use rowlark_core::{Driver, QueryEvent, Value};
use tokio::sync::mpsc;

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_million_rows_stream_in_batches_without_loss() {
    let driver = pg16().await;
    let started = Instant::now();
    let (tx, mut rx) = mpsc::channel(4);
    let collect = async {
        let (mut rows, mut largest_batch, mut sum, mut next) = (0u64, 0usize, 0i64, 1i64);
        while let Some(event) = rx.recv().await {
            if let QueryEvent::Rows(batch) = event {
                largest_batch = largest_batch.max(batch.len());
                rows += batch.len() as u64;
                for row in batch {
                    // Rows arrive in order, none missing or repeated.
                    let Value::Int(n) = row[0] else {
                        panic!("{row:?}")
                    };
                    assert_eq!(n, next);
                    next += 1;
                    sum += n;
                }
            }
        }
        (rows, largest_batch, sum)
    };
    let (result, (rows, largest_batch, sum)) = tokio::join!(
        driver.run_query(
            "SELECT g, 'row ' || g AS label FROM generate_series(1, 1000000) g",
            tx
        ),
        collect
    );
    let elapsed = started.elapsed();
    assert_eq!(result.expect("query").rows_returned, 1_000_000);
    assert_eq!(rows, 1_000_000);
    assert_eq!(sum, 1_000_000 * 1_000_001 / 2);
    // Never more than a batch at a time, so memory stays flat.
    assert_eq!(largest_batch, 1000);
    eprintln!(
        "1,000,000 rows in {elapsed:?} ({:.0} rows/s)",
        1e6 / elapsed.as_secs_f64()
    );
    assert!(elapsed < Duration::from_secs(60), "{elapsed:?}");
}

/// If the UI reads slowly, the driver must wait for it instead of piling the
/// whole result up in memory.
#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_slow_reader_makes_the_driver_wait() {
    let driver = Arc::new(pg16().await);
    let producer = Arc::clone(&driver);
    let (tx, mut rx) = mpsc::channel(2);
    let query = tokio::spawn(async move {
        producer
            .run_query("SELECT g FROM generate_series(1, 200000) g", tx)
            .await
    });

    // Read one batch, then stop reading for a while.
    assert!(matches!(rx.recv().await, Some(QueryEvent::Columns(_))));
    assert!(matches!(rx.recv().await, Some(QueryEvent::Rows(_))));
    tokio::time::sleep(Duration::from_millis(700)).await;
    // 200 batches are waiting to be sent, yet the query is still running:
    // it is blocked on the full channel, holding at most a few batches.
    assert!(
        !query.is_finished(),
        "the driver ran ahead of a reader that was not reading"
    );

    // Resume reading: everything arrives.
    let mut rows = 1000u64;
    while let Some(event) = rx.recv().await {
        if let QueryEvent::Rows(batch) = event {
            rows += batch.len() as u64;
        }
    }
    assert_eq!(rows, 200_000);
    assert_eq!(query.await.unwrap().unwrap().rows_returned, 200_000);
}
