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
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
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
            .query_one_raw(Statement::from_sql_and_values(
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
             VALUES ('{}', '{sensor_id}', 1.0, 0.0, '2000-01-01T00:00:00Z', 'bench base')",
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
        .query_one_raw(Statement::from_sql_and_values(
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

#[tokio::test]
#[serial]
async fn rerun_replays_a_backdate_from_its_stored_params() {
    let (db, app, token) = setup().await;

    let job_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status, category, params, completed_at) \
             VALUES ('{job_id}', 'reprocess_all', 'completed', 'operator', '{{}}'::jsonb, NOW())"
        ),
    )
    .await;

    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/reprocessing_jobs/{job_id}/rerun"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "rerun ({status}): {text}");
    let rerun = job_id_of(&text);
    assert_ne!(rerun, job_id.to_string(), "the rerun is a new row");

    assert_eq!(wait_for_terminal(&db, &rerun).await, "completed");
    crate::common::cleanup_test_db(&db).await;
}

/// Two `pairing_backfill` rows differ only by the slot in their `params`, so a guard comparing
/// `sensor_id` and `trigger_id` (both NULL on every one of them) reads them as the same job.
#[tokio::test]
#[serial]
async fn rerun_pairing_backfill_ignores_an_in_flight_row_for_another_slot() {
    let (db, app, token) = setup().await;

    let site = crate::common::SITE1_ID;
    let done = Uuid::new_v4();
    let queued = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status, category, params, completed_at) \
             VALUES ('{done}', 'pairing_backfill', 'completed', 'operator', \
                     '{{\"site_id\": \"{site}\", \"parameter_id\": \"{}\"}}'::jsonb, NOW())",
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status, category, params, next_attempt_at) \
             VALUES ('{queued}', 'pairing_backfill', 'queued', 'operator', \
                     '{{\"site_id\": \"{site}\", \"parameter_id\": \"{}\"}}'::jsonb, NOW() + interval '1 hour')",
            crate::common::GLOBAL_PARAM_DEPTH_ID
        ),
    )
    .await;

    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/reprocessing_jobs/{done}/rerun"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "a queued backfill for another slot does not block this rerun ({status}): {text}"
    );
    crate::common::cleanup_test_db(&db).await;
}

/// The Rerun and Cancel buttons follow the server's policy, so the row states it rather than the
/// client keeping a copy of the two lists.
#[tokio::test]
#[serial]
async fn a_job_row_states_whether_it_can_be_rerun_and_cancelled() {
    let (db, app, token) = setup().await;

    let recompute = Uuid::new_v4();
    let import = Uuid::new_v4();
    for (id, trigger_type) in [(recompute, "event_recompute"), (import, "csv_import")] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO reprocessing_jobs (id, trigger_type, status, category, completed_at) \
                 VALUES ('{id}', '{trigger_type}', 'completed', 'operator', NOW())"
            ),
        )
        .await;
    }

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/reprocessing_jobs/{recompute}"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["rerunnable"], serde_json::json!(true));
    assert_eq!(body["cancellable"], serde_json::json!(true));

    let (status, body) =
        crate::common::get_json_with_token(&app, &format!("/api/reprocessing_jobs/{import}"), &token)
            .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["rerunnable"], serde_json::json!(false));
    assert_eq!(body["cancellable"], serde_json::json!(true));

    let (status, body) =
        crate::common::get_json_with_token(&app, "/api/reprocessing_jobs?sort=created_at", &token)
            .await;
    assert_eq!(status, 200, "{body}");
    let listed = body
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["id"] == serde_json::json!(recompute.to_string()))
        .expect("the recompute row is listed");
    assert_eq!(listed["rerunnable"], serde_json::json!(true));
    assert_eq!(listed["cancellable"], serde_json::json!(true));

    crate::common::cleanup_test_db(&db).await;
}
