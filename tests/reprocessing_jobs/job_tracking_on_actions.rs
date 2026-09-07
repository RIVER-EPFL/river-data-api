//! E2E tests for tracked background actions.
//!
//! Covers the action endpoints that enqueue a worker-pool job and therefore
//! create a visible `reprocessing_jobs` row: `/actions/reprocess`,
//! `/actions/refresh_aggregates`, and `/actions/compute_derived`.
//!
//! Run: DATABASE_URL=postgresql://postgres:psql@localhost:5444/river_test \
//!      cargo test --test reprocessing_jobs -- --test-threads=1

use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;
use uuid::Uuid;


async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

/// Number of `reprocessing_jobs` rows with the given id (0 or 1).
async fn job_exists(db: &sea_orm::DatabaseConnection, job_id: &str) -> bool {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COUNT(*) AS n FROM reprocessing_jobs WHERE id = $1",
            [Uuid::parse_str(job_id).unwrap().into()],
        ))
        .await
        .unwrap()
        .unwrap();
    let n: i64 = row.try_get("", "n").unwrap();
    n == 1
}

fn job_id_of(text: &str) -> String {
    let json: serde_json::Value = serde_json::from_str(text).unwrap_or_else(|_| {
        panic!("response was not JSON: {text}");
    });
    json["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("response missing job_id: {json}"))
        .to_string()
}

#[tokio::test]
#[serial]
async fn reprocess_creates_tracked_job_for_seeded_sensor() {
    let (db, app, token) = setup().await;

    let sensor_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, is_active) \
             VALUES ('{sensor_id}', 'Reprocess-Probe', true)"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensor_calibrations (id, sensor_id, slope, intercept, valid_from, notes) \
             VALUES ('{}', '{sensor_id}', 1.0, 0.0, '2000-01-01T00:00:00Z', 'bench base')",
            Uuid::new_v4()
        ),
    )
    .await;

    let body = serde_json::json!({ "sensor_id": sensor_id.to_string() });
    let (status, text) =
        crate::common::post_json_with_token(&app, "/api/actions/reprocess", &body, &token).await;

    assert!(
        (200..300).contains(&status),
        "reprocess should return 2xx, got {status}: {text}"
    );
    let job_id = job_id_of(&text);
    assert!(
        job_exists(&db, &job_id).await,
        "a reprocessing_jobs row should exist for job_id {job_id}"
    );

    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT trigger_type, sensor_id FROM reprocessing_jobs WHERE id = $1",
            [Uuid::parse_str(&job_id).unwrap().into()],
        ))
        .await
        .unwrap()
        .unwrap();
    let trigger_type: String = row.try_get("", "trigger_type").unwrap();
    let row_sensor_id: Uuid = row.try_get("", "sensor_id").unwrap();
    assert_eq!(trigger_type, "manual_reprocess");
    assert_eq!(row_sensor_id, sensor_id);

    crate::common::jobs::wait_for_job(&db, &job_id).await;
    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn refresh_aggregates_creates_tracked_job() {
    let (db, app, token) = setup().await;

    let body = serde_json::json!({ "full": false });
    let (status, text) =
        crate::common::post_json_with_token(&app, "/api/actions/refresh_aggregates", &body, &token)
            .await;

    assert!(
        (200..300).contains(&status),
        "refresh_aggregates should return 2xx, got {status}: {text}"
    );
    let job_id = job_id_of(&text);
    assert!(
        job_exists(&db, &job_id).await,
        "refresh_aggregates should create a tracked reprocessing_jobs row for {job_id}"
    );

    let terminal = crate::common::jobs::wait_for_job(&db, &job_id).await;
    assert_eq!(
        terminal, "completed",
        "incremental refresh should complete, not fail"
    );
    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn compute_derived_creates_tracked_job() {
    let (db, app, token) = setup().await;

    let body = serde_json::json!({
        "site_timestamps": [
            {
                "site_id": crate::common::SITE1_ID,
                "timestamps": ["2025-01-15T00:00:00Z", "2025-01-15T00:10:00Z"]
            }
        ]
    });
    let (status, text) =
        crate::common::post_json_with_token(&app, "/api/actions/compute_derived", &body, &token)
            .await;

    assert!(
        (200..300).contains(&status),
        "compute_derived should return 2xx, got {status}: {text}"
    );
    let job_id = job_id_of(&text);
    assert!(
        job_exists(&db, &job_id).await,
        "compute_derived should create a tracked reprocessing_jobs row for {job_id}"
    );

    let terminal = crate::common::jobs::wait_for_job(&db, &job_id).await;
    assert_eq!(
        terminal, "completed",
        "compute_derived should complete, not fail"
    );
    crate::common::cleanup_test_db(&db).await;
}
