//! Tracked-job automatic retry: a transiently-failing job retries with backoff before completing,
//! and a persistently-failing job exhausts its retries and is marked failed. Drives
//! `spawn_tracked_job_with_retry` directly with a controllable factory + tiny backoff so the test is
//! deterministic and fast.
//!
//! Run: cargo test --test job_retry_test -- --test-threads=1

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use river_db::routes::private::sensor_calibrations::services::spawn_tracked_job_with_retry;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, Statement};
use serial_test::serial;

fn events() -> river_db::common::EventSender {
    tokio::sync::broadcast::channel::<river_db::common::AppEvent>(16).0
}

async fn wait_until_terminal(
    db: &DatabaseConnection,
    job_id: uuid::Uuid,
) -> (String, Option<i32>, i32) {
    for _ in 0..400 {
        let row = db
            .query_one(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT status, readings_updated, retry_count FROM reprocessing_jobs WHERE id = '{job_id}'"
                ),
            ))
            .await
            .unwrap();
        if let Some(row) = row {
            let status: String = row.try_get("", "status").unwrap();
            if status == "completed" || status == "failed" {
                let readings_updated: Option<i32> = row.try_get("", "readings_updated").unwrap();
                let retry_count: i32 = row.try_get("", "retry_count").unwrap();
                return (status, readings_updated, retry_count);
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("job {job_id} did not reach a terminal state");
}

#[tokio::test]
#[serial]
async fn job_retries_then_succeeds() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;

    let attempts = Arc::new(AtomicU32::new(0));
    let a = attempts.clone();
    // Fail the first two attempts, succeed on the third.
    let make_work = move |_db: DatabaseConnection| {
        let a = a.clone();
        async move {
            if a.fetch_add(1, Ordering::SeqCst) < 2 {
                Err(DbErr::Custom("transient boom".into()))
            } else {
                Ok(7)
            }
        }
    };

    let job_id = spawn_tracked_job_with_retry(
        &db,
        None,
        "test_retry",
        None,
        events(),
        3,
        Duration::from_millis(10),
        make_work,
    )
    .await
    .unwrap();

    let (status, readings_updated, retry_count) = wait_until_terminal(&db, job_id).await;
    assert_eq!(status, "completed", "should succeed after retrying");
    assert_eq!(readings_updated, Some(7), "completed job records the work's count");
    assert_eq!(retry_count, 2, "two failed attempts before success");
    assert_eq!(attempts.load(Ordering::SeqCst), 3, "work invoked three times total");
}

#[tokio::test]
#[serial]
async fn job_exhausts_retries_then_fails() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;

    let attempts = Arc::new(AtomicU32::new(0));
    let a = attempts.clone();
    let make_work = move |_db: DatabaseConnection| {
        let a = a.clone();
        async move {
            a.fetch_add(1, Ordering::SeqCst);
            Err::<i64, _>(DbErr::Custom("always boom".into()))
        }
    };

    let job_id = spawn_tracked_job_with_retry(
        &db,
        None,
        "test_retry",
        None,
        events(),
        2,
        Duration::from_millis(10),
        make_work,
    )
    .await
    .unwrap();

    let (status, _readings_updated, retry_count) = wait_until_terminal(&db, job_id).await;
    assert_eq!(status, "failed", "should fail once retries are exhausted");
    assert_eq!(retry_count, 2, "two retries attempted");
    assert_eq!(attempts.load(Ordering::SeqCst), 3, "initial attempt + two retries");
}
