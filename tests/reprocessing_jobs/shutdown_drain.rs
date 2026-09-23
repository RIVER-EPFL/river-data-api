//! Shutdown stops the worker claiming new work; it does not abandon the job already claimed. A pod
//! rolled mid-run would otherwise leave a part-written job holding its lease until the reaper takes
//! it, and the loop's own doc comment promises the opposite of what a cancelled future does.
//!
//! Run: cargo test --test reprocessing_jobs shutdown_drain -- --test-threads=1

use std::sync::Arc;
use std::time::Duration;

use river_db::routes::private::reprocessing_jobs::service as jobs;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use crate::common::jobs::{ClosureJob, registry_of};

/// How long the job stays in flight after announcing itself, so the shutdown lands mid-run.
const WORK: Duration = Duration::from_millis(300);

async fn job_row(db: &DatabaseConnection, job_id: uuid::Uuid) -> (String, Option<i32>) {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT status, readings_updated FROM reprocessing_jobs WHERE id = '{job_id}'"),
        ))
        .await
        .unwrap()
        .expect("the enqueued job row");
    (
        row.try_get("", "status").unwrap(),
        row.try_get("", "readings_updated").unwrap(),
    )
}

#[tokio::test]
#[serial]
async fn shutdown_lets_the_claimed_job_finish() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let started_tx = std::sync::Mutex::new(Some(started_tx));
    let job = ClosureJob::new("test_retry", move |_ctx| {
        let announce = started_tx.lock().unwrap().take();
        async move {
            if let Some(tx) = announce {
                let _ = tx.send(());
            }
            tokio::time::sleep(WORK).await;
            Ok(5)
        }
    });

    let events = tokio::sync::broadcast::channel(16).0;
    let registry = Arc::new(registry_of(job));
    let job_id = jobs::enqueue(&db, "test_retry", None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .expect("a fresh enqueue inserts a row");

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let worker_task = tokio::spawn({
        let db = db.clone();
        async move {
            jobs::run_workers(db, events, registry, async move {
                let _ = stop_rx.await;
            })
            .await;
        }
    });

    // Signal shutdown while the job is in flight, which is what a rollout does.
    started_rx.await.expect("the job announces itself");
    let _ = stop_tx.send(());

    tokio::time::timeout(Duration::from_secs(10), worker_task)
        .await
        .expect("the worker returns after shutdown")
        .expect("the worker task does not panic");

    let (status, readings_updated) = job_row(&db, job_id).await;
    assert_eq!(
        status, "completed",
        "the claimed job finishes rather than being cancelled mid-run"
    );
    assert_eq!(
        readings_updated,
        Some(5),
        "the finished job records what it did"
    );
}

/// The process's own stop: the HTTP server returns as soon as the signal lands when no request is
/// in flight, and whatever follows it decides whether the claimed job finishes before exit.
#[tokio::test]
#[serial]
async fn server_stop_waits_for_the_claimed_job() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let started_tx = std::sync::Mutex::new(Some(started_tx));
    let job = ClosureJob::new("test_retry", move |_ctx| {
        let announce = started_tx.lock().unwrap().take();
        async move {
            if let Some(tx) = announce {
                let _ = tx.send(());
            }
            tokio::time::sleep(WORK).await;
            Ok(7)
        }
    });

    let events = tokio::sync::broadcast::channel(16).0;
    let registry = Arc::new(registry_of(job));
    let job_id = jobs::enqueue(&db, "test_retry", None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .expect("a fresh enqueue inserts a row");

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn({
        let db = db.clone();
        let mut shutdown_rx = shutdown_rx.clone();
        async move {
            jobs::run_workers(db, events, registry, async move {
                let _ = shutdown_rx.changed().await;
            })
            .await;
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut server_shutdown = shutdown_rx.clone();
    let server = axum::serve(listener, axum::Router::new()).with_graceful_shutdown(async move {
        let _ = server_shutdown.changed().await;
    });

    started_rx.await.expect("the job announces itself");
    let _ = shutdown_tx.send(true);

    tokio::time::timeout(
        Duration::from_secs(10),
        jobs::serve_then_drain_worker(
            server.into_future(),
            shutdown_rx,
            worker,
            Duration::from_secs(5),
        ),
    )
    .await
    .expect("the stop returns once the worker has drained")
    .expect("the server stops cleanly");

    let (status, readings_updated) = job_row(&db, job_id).await;
    assert_eq!(
        status, "completed",
        "the process does not exit while its claimed job is still writing"
    );
    assert_eq!(readings_updated, Some(7));
}
