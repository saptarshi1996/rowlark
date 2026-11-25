//! Several things happening at once: many connections, cancel racing with a
//! query finishing, and a query task being thrown away mid-stream.
//!
//! Needs `docker compose up -d`; run with:
//!
//!     cargo test -p rowlark-core --test postgres_concurrency -- --ignored

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{PG16, config, exec, pg16, run};
use rowlark_core::{Driver, DriverError, PostgresDriver, QueryEvent, TransactionState, Value};
use tokio::sync::mpsc;

#[tokio::test]
#[ignore = "needs docker compose"]
async fn many_connections_at_once_each_get_their_own_answers() {
    let mut tasks = Vec::new();
    for n in 0..24_i64 {
        tasks.push(tokio::spawn(async move {
            let d = pg16().await;
            // Each connection asks for its own number several times, with a
            // short random-ish pause, so the queries interleave on the server.
            for round in 0..5_i64 {
                tokio::time::sleep(Duration::from_millis(((n * 7 + round * 3) % 11) as u64)).await;
                let r = run(
                    &d,
                    &format!("SELECT {n}::int8 * 100 + {round}, pg_backend_pid()"),
                )
                .await;
                let row = r.rows()[0].clone();
                assert_eq!(
                    row[0],
                    Value::Int(n * 100 + round),
                    "wrong answer on connection {n}"
                );
            }
        }));
    }
    for t in tasks {
        t.await.expect("a connection task panicked");
    }
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn cancel_racing_with_the_end_of_a_query_never_poisons_the_next_one() {
    let d = Arc::new(pg16().await);
    let mut cancelled = 0;
    let mut finished = 0;
    // The query takes about 150 ms. A cancel needs its own connection, which
    // takes a few milliseconds to open, so cancelling at 0..220 ms lands
    // before the query ends, around its end, and after it ended.
    for delay_ms in (0..220).step_by(10) {
        let background = Arc::clone(&d);
        let query = tokio::spawn(async move { run(&background, "SELECT pg_sleep(0.15)").await });
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        d.cancel().await.expect("cancel must not fail");
        let outcome = query.await.expect("task panicked").result;
        match outcome {
            Ok(_) => finished += 1,
            Err(DriverError::Cancelled) => cancelled += 1,
            other => panic!("after a {delay_ms} ms delay: {other:?}"),
        }

        // Whatever happened, the very next query on this connection works and
        // is not hit by a late cancel.
        let next = run(&d, "SELECT 'still fine'").await;
        assert_eq!(
            next.scalar(),
            &Value::Text("still fine".into()),
            "after a {delay_ms} ms delay"
        );
    }
    assert!(
        cancelled > 0,
        "no cancel ever landed; the test is not testing anything"
    );
    assert!(finished > 0, "no query ever finished before the cancel");
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn many_cancels_at_once_are_all_harmless() {
    let d = Arc::new(pg16().await);
    let background = Arc::clone(&d);
    let query = tokio::spawn(async move { run(&background, "SELECT pg_sleep(30)").await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut cancels = Vec::new();
    for _ in 0..10 {
        let d = Arc::clone(&d);
        cancels.push(tokio::spawn(async move { d.cancel().await }));
    }
    for c in cancels {
        c.await.unwrap().expect("a cancel failed");
    }
    let outcome = tokio::time::timeout(Duration::from_secs(5), query)
        .await
        .expect("the query did not stop")
        .unwrap();
    assert!(matches!(outcome.result, Err(DriverError::Cancelled)));
    assert_eq!(run(&d, "SELECT 1").await.scalar(), &Value::Int(1));
}

/// How many backends with this application name are running a query right now.
async fn active_backends(watcher: &PostgresDriver, name: &str) -> i64 {
    let r = exec(
        watcher,
        &format!(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE application_name = '{name}' AND state = 'active' \
               AND pid <> pg_backend_pid()"
        ),
    )
    .await;
    match r.scalar() {
        Value::Int(n) => *n,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_task_thrown_away_mid_stream_leaves_the_driver_usable() {
    // This is what happens when a tab is closed: the task running the query is
    // dropped while rows are still arriving. The connection must not be left
    // locked, "busy", or full of half-read rows.
    let d = Arc::new(pg16().await);
    let background = Arc::clone(&d);
    let task = tokio::spawn(async move {
        let (tx, mut rx) = mpsc::channel::<QueryEvent>(1);
        let reader = async {
            // Read a little, then stall, so the driver is blocked mid-stream.
            let _ = rx.recv().await;
            let _ = rx.recv().await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        };
        let (result, ()) = tokio::join!(
            background.run_query("SELECT g FROM generate_series(1, 20000000) g", tx),
            reader
        );
        result
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    task.abort();
    let _ = task.await; // JoinError::Cancelled is expected

    // The same driver, right away: not stuck "busy", and able to run again.
    let started = Instant::now();
    let after = tokio::time::timeout(Duration::from_secs(20), run(&d, "SELECT 'back'"))
        .await
        .expect("the driver hung after its query task was dropped");
    assert_eq!(
        after.scalar(),
        &Value::Text("back".into()),
        "{:?}",
        after.result
    );
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "recovering took {:?}",
        started.elapsed()
    );
    assert_eq!(d.transaction_state().await.unwrap(), TransactionState::Idle);
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn dropping_the_driver_mid_stream_stops_the_work_on_the_server() {
    let name = "rowlark_drop_test";
    let mut cfg = config(PG16);
    cfg.application_name = name.to_string();
    let victim = Arc::new(
        PostgresDriver::connect(&cfg, "rowlark")
            .await
            .expect("connect"),
    );
    let watcher = pg16().await;

    let background = Arc::clone(&victim);
    let task = tokio::spawn(async move {
        let (tx, mut rx) = mpsc::channel::<QueryEvent>(1);
        let reader = async { while rx.recv().await.is_some() {} };
        let (result, ()) = tokio::join!(
            // Slow on the server side, so it is still running when we look.
            background.run_query("SELECT pg_sleep(60)", tx),
            reader
        );
        result
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        active_backends(&watcher, name).await,
        1,
        "the query never started"
    );

    // Throw away the task and the driver together.
    task.abort();
    let _ = task.await;
    drop(victim);

    // The server notices the closed connection and stops the query.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if active_backends(&watcher, name).await == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the server is still running the query of a dropped driver"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn after_a_query_task_is_aborted_the_next_query_does_not_wait_for_it() {
    // The abandoned query would run for 20 seconds. The driver must tell the
    // server to stop it, so the next query on the same connection is quick.
    let d = Arc::new(pg16().await);
    let background = Arc::clone(&d);
    let task = tokio::spawn(async move { run(&background, "SELECT pg_sleep(20)").await });
    tokio::time::sleep(Duration::from_millis(400)).await;
    task.abort();
    let _ = task.await;

    let started = Instant::now();
    let next = run(&d, "SELECT 'quick'").await;
    assert_eq!(
        next.scalar(),
        &Value::Text("quick".into()),
        "{:?}",
        next.result
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the next query waited {:?} for the abandoned one",
        started.elapsed()
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn a_timeout_around_a_query_stops_it_on_the_server_too() {
    // Wrapping `run_query` in `tokio::time::timeout` is the obvious way for a
    // UI to give up on a query; when it fires, the query future is dropped.
    let d = pg16().await;
    let (tx, mut rx) = mpsc::channel::<QueryEvent>(4);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let outcome = tokio::time::timeout(
        Duration::from_millis(300),
        d.run_query("SELECT pg_sleep(20)", tx),
    )
    .await;
    assert!(outcome.is_err(), "the timeout should have fired first");
    drain.abort();

    let started = Instant::now();
    assert_eq!(run(&d, "SELECT 1").await.scalar(), &Value::Int(1));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
#[ignore = "needs docker compose"]
async fn dropping_a_query_that_is_still_waiting_for_its_turn_cancels_nobody_else() {
    // Two queries share a connection. The second waits for the first. If the
    // second is dropped while waiting, the first must carry on untouched.
    let d = Arc::new(pg16().await);
    let first = Arc::clone(&d);
    let long = tokio::spawn(async move { run(&first, "SELECT pg_sleep(1), 'first done'").await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let second = Arc::clone(&d);
    let waiting = tokio::spawn(async move { run(&second, "SELECT 'second'").await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    waiting.abort();
    let _ = waiting.await;

    let done = long.await.expect("task panicked");
    assert!(
        done.result.is_ok(),
        "the first query was cancelled: {:?}",
        done.result
    );
    assert_eq!(done.rows()[0][1], Value::Text("first done".into()));
}
