//! Cooperative cancellation: a running cancellable job observes the cancel flag at its loop
//! checkpoint and finishes as `cancelled`. The cancel endpoint sets a durable flag so a job owned by
//! any replica stops; non-cancellable types and terminal jobs report 409.
//!
//! Run: cargo test --test reprocessing_jobs -- --test-threads=1

use river_db::routes::private::reprocessing_jobs::lifecycle::{request_cancel, spawn_tracked_job_ctx};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use std::time::{Duration, Instant};
use uuid::Uuid;

fn events() -> river_db::common::EventSender {
    tokio::sync::broadcast::channel::<river_db::common::AppEvent>(16).0
}

async fn status_of(db: &DatabaseConnection, id: Uuid) -> String {
    db.query_one(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT status FROM reprocessing_jobs WHERE id = $1",
        [id.into()],
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<String>("", "status")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn cancelling_a_running_job_marks_it_cancelled() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    // A cancellable job that spins until the cancel flag flips.
    let job_id = spawn_tracked_job_ctx(
        &db,
        None,
        "csv_import",
        None,
        events(),
        |ctx| async move {
            while !ctx.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Ok(0)
        },
    )
    .await
    .unwrap();

    // Request cancel once the job has registered its flag (it registers at task start).
    let start = Instant::now();
    while !request_cancel(job_id) {
        assert!(start.elapsed() < Duration::from_secs(5), "job never registered");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // It should settle as cancelled.
    let start = Instant::now();
    loop {
        if status_of(&db, job_id).await == "cancelled" {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "job did not cancel");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn cancel_endpoint_rejects_non_cancellable_and_unknown() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    // Non-cancellable type (single-statement refresh) -> 409.
    let refresh = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status, category) \
             VALUES ('{refresh}', 'refresh_aggregates', 'running', 'maintenance')"
        ),
    )
    .await;
    let (status, _t) = crate::common::post_json_with_token(
        &app,
        &format!("/api/reprocessing_jobs/{refresh}/cancel"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 409, "refresh_aggregates is not cancellable");

    // Cancellable running job: accepted regardless of which replica owns it -> 200, flag set.
    let running = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status, category) \
             VALUES ('{running}', 'csv_import', 'running', 'operator')"
        ),
    )
    .await;
    let (status, _t) = crate::common::post_json_with_token(
        &app,
        &format!("/api/reprocessing_jobs/{running}/cancel"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "a running cancellable job accepts a cross-replica cancel");
    let flagged: bool = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT cancel_requested FROM reprocessing_jobs WHERE id = $1",
            [running.into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "cancel_requested")
        .unwrap();
    assert!(flagged, "cancel_requested is set for the owning replica's heartbeat to observe");

    // A terminal job is not in a cancellable state -> 409.
    let done = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status, category) \
             VALUES ('{done}', 'csv_import', 'completed', 'operator')"
        ),
    )
    .await;
    let (status, _t) = crate::common::post_json_with_token(
        &app,
        &format!("/api/reprocessing_jobs/{done}/cancel"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 409, "a completed job cannot be cancelled");

    // Unknown id -> 404.
    let (status, _t) = crate::common::post_json_with_token(
        &app,
        &format!("/api/reprocessing_jobs/{}/cancel", Uuid::new_v4()),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 404);

    crate::common::cleanup_test_db(&db).await;
}
