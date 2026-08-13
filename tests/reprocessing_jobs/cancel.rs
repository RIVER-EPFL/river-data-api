//! The cancel endpoint sets a durable `cancel_requested` flag so a running cancellable job owned by
//! any replica is stopped by its owning replica's heartbeat; a still-`queued` job is cancelled
//! outright. Non-cancellable types, terminal jobs, and unknown ids report 409/404. (The worker
//! honoring the flag is the heartbeat path in `worker.rs`; it is not exercised here because the
//! heartbeat cadence is 40s, the fast, deterministic contract is that the flag gets set.)
//!
//! Run: cargo test --test reprocessing_jobs -- --test-threads=1

use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;
use uuid::Uuid;

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
    assert_eq!(
        status, 200,
        "a running cancellable job accepts a cross-replica cancel"
    );
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
    assert!(
        flagged,
        "cancel_requested is set for the owning replica's heartbeat to observe"
    );

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
