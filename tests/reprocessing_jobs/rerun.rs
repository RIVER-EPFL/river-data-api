//! Rerunning a finished job replays it from the ids stored on its row, producing a NEW job while
//! the original is preserved. Non-rerunnable types are rejected with 409.
//!
//! Run: cargo test --test reprocessing_jobs -- --test-threads=1

use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;
use std::time::{Duration, Instant};
use uuid::Uuid;

const WAIT_TIMEOUT: Duration = Duration::from_secs(10);

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

fn job_id_of(text: &str) -> String {
    let json: serde_json::Value = serde_json::from_str(text).unwrap();
    json["job_id"].as_str().unwrap().to_string()
}

async fn wait_for_terminal(db: &sea_orm::DatabaseConnection, job_id: &str) -> String {
    let id = Uuid::parse_str(job_id).unwrap();
    let start = Instant::now();
    loop {
        let row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT status FROM reprocessing_jobs WHERE id = $1",
                [id.into()],
            ))
            .await
            .unwrap()
            .expect("job row exists");
        let status: String = row.try_get("", "status").unwrap();
        if status != "queued" && status != "pending" && status != "running" && status != "retrying"
        {
            return status;
        }
        if start.elapsed() > WAIT_TIMEOUT {
            panic!("job {job_id} did not settle");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
#[serial]
async fn rerun_replays_a_sensor_reprocess_as_a_new_job() {
    let (db, app, token) = setup().await;

    let sensor_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, is_active) \
             VALUES ('{sensor_id}', 'Rerun-Probe', true)"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensor_calibrations (id, sensor_id, slope, intercept, valid_from, notes) \
             VALUES ('{}', '{sensor_id}', 1.0, 0.0, '2000-01-01T00:00:00Z', 'identity')",
            Uuid::new_v4()
        ),
    )
    .await;

    let body = serde_json::json!({ "sensor_id": sensor_id.to_string() });
    let (_s, text) =
        crate::common::post_json_with_token(&app, "/api/actions/reprocess", &body, &token).await;
    let original = job_id_of(&text);
    wait_for_terminal(&db, &original).await;

    // Rerun the finished job.
    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/reprocessing_jobs/{original}/rerun"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "rerun should be 2xx, got {status}: {text}"
    );
    let rerun = job_id_of(&text);
    assert_ne!(rerun, original, "rerun must create a NEW job");

    // The original row is preserved; the new one replays the same trigger_type + sensor.
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT trigger_type, sensor_id FROM reprocessing_jobs WHERE id = $1",
            [Uuid::parse_str(&rerun).unwrap().into()],
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.try_get::<String>("", "trigger_type").unwrap(),
        "manual_reprocess"
    );
    assert_eq!(row.try_get::<Uuid>("", "sensor_id").unwrap(), sensor_id);

    assert_eq!(wait_for_terminal(&db, &rerun).await, "completed");
    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn rerun_rejects_non_rerunnable_type() {
    let (db, app, token) = setup().await;

    let job_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status, category, completed_at) \
             VALUES ('{job_id}', 'csv_import', 'completed', 'operator', NOW())"
        ),
    )
    .await;

    let (status, _text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/reprocessing_jobs/{job_id}/rerun"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 409, "csv_import is not rerunnable");
    crate::common::cleanup_test_db(&db).await;
}
