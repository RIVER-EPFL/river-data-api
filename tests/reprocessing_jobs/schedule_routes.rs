//! The schedule control-plane REST surface (Stage D): list/get/PATCH/run_now/audit. PATCH validates
//! interval/policies/tunables, recomputes `next_run_at` on a cadence change, and writes an audit
//! row; run_now enqueues one off-cadence run for a known job. `running` reflects an in-flight job.
//!
//! Run: cargo test --test reprocessing_jobs -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

/// `janitor_service` is the recurring Service that carries a real `validate` (the `retention_days`
/// tunable), so it's the case the tunables tests target. It is in the full registry (build_registry
/// + register_scheduled_services), which the handlers rebuild per call.
const JOB: &str = "janitor_service";

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

/// Insert one schedule row directly (deterministic — no dependence on which Services seed).
async fn insert_schedule(db: &DatabaseConnection, job_name: &str, interval_seconds: i64) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO schedules (job_name, enabled, next_run_at, interval_seconds, overlap_policy, catchup_policy) \
             VALUES ('{job_name}', true, now() + interval '{interval_seconds} seconds', {interval_seconds}, 'skip_if_running', 'run_once')"
        ),
    )
    .await;
}

async fn next_run_at(db: &DatabaseConnection, job_name: &str) -> chrono::DateTime<chrono::Utc> {
    db.query_one(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT next_run_at FROM schedules WHERE job_name = $1",
        [job_name.into()],
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<chrono::DateTime<chrono::Utc>>("", "next_run_at")
    .unwrap()
}

async fn audit_count(db: &DatabaseConnection, job_name: &str) -> i64 {
    db.query_one(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT count(*) AS n FROM schedule_audit WHERE job_name = $1",
        [job_name.into()],
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn patch_recomputes_next_run_on_interval_change() {
    let (db, app, token) = setup().await;
    insert_schedule(&db, JOB, 3600).await;

    let before = next_run_at(&db, JOB).await;

    // Lower the interval — next_run_at should snap to now + the new (shorter) interval.
    let (status, body) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/schedules/{JOB}"),
        &serde_json::json!({ "interval_seconds": 60 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "PATCH should succeed: {body}");

    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(view["interval_seconds"], 60);

    let after = next_run_at(&db, JOB).await;
    assert!(after < before, "lowered interval moves next_run_at earlier (was {before}, now {after})");
    let expected = chrono::Utc::now() + chrono::Duration::seconds(60);
    assert!(
        (after - expected).num_seconds().abs() < 10,
        "next_run_at is ~now + 60s (got {after}, expected ~{expected})"
    );
    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn patch_interval_zero_is_rejected() {
    let (db, app, token) = setup().await;
    insert_schedule(&db, JOB, 3600).await;

    let (status, _body) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/schedules/{JOB}"),
        &serde_json::json!({ "interval_seconds": 0 }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "interval_seconds < 1 is a 400");
    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn patch_unknown_job_name_is_404() {
    let (db, app, token) = setup().await;
    // No row for this job_name.
    let (status, _body) = crate::common::patch_json_with_token(
        &app,
        "/api/schedules/no_such_service",
        &serde_json::json!({ "enabled": false }),
        &token,
    )
    .await;
    assert_eq!(status, 404, "PATCH of a non-existent schedule is a 404");
    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn patch_invalid_tunables_is_400_with_message() {
    let (db, app, token) = setup().await;
    insert_schedule(&db, JOB, 3600).await;

    let (status, body) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/schedules/{JOB}"),
        &serde_json::json!({ "tunables": { "retention_days": -5 } }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "negative retention_days is rejected by the Job validate");
    assert!(
        body.contains("retention_days"),
        "the 400 surfaces the validate message: {body}"
    );

    // The rejected edit did not persist and wrote no audit row.
    assert_eq!(audit_count(&db, JOB).await, 0, "a rejected PATCH writes no audit row");
    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn patch_valid_tunables_persist_and_audit_row_written() {
    let (db, app, token) = setup().await;
    insert_schedule(&db, JOB, 3600).await;

    let (status, body) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/schedules/{JOB}"),
        &serde_json::json!({ "tunables": { "retention_days": 30 } }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "valid retention_days is accepted: {body}");

    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(view["tunables"]["retention_days"], 30);
    // The token actor is stamped on updated_by.
    assert!(
        view["updated_by"].as_str().is_some_and(|s| s.starts_with("token:")),
        "updated_by is the calling principal: {}",
        view["updated_by"]
    );

    // Exactly one audit row with the before/after snapshot.
    assert_eq!(audit_count(&db, JOB).await, 1, "one accepted PATCH writes one audit row");

    let (astatus, abody) =
        crate::common::get_json_with_token(&app, &format!("/api/schedules/{JOB}/audit"), &token)
            .await;
    assert_eq!(astatus, 200);
    let entries = abody.as_array().expect("audit is a list");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["new_value"]["tunables"]["retention_days"], 30);
    assert!(
        entries[0]["old_value"]["tunables"].is_object(),
        "old_value snapshot has the pre-image tunables"
    );
    assert!(
        entries[0]["changed_by"].as_str().is_some_and(|s| s.starts_with("token:")),
        "changed_by records the principal"
    );
    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn run_now_enqueues_for_known_job_and_404s_for_unknown() {
    let (db, app, token) = setup().await;
    insert_schedule(&db, JOB, 3600).await;

    // Known job → enqueued, with a job_id.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/schedules/{JOB}/run_now"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "run_now of a known job succeeds: {body}");
    let resp: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(resp["enqueued"], true, "a fresh run_now enqueues a job");
    assert!(resp["job_id"].as_str().is_some(), "an enqueued run returns a job_id");

    // An enqueued queued row of this trigger_type exists.
    let n: i64 = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*) AS n FROM reprocessing_jobs WHERE trigger_type = $1",
            [JOB.into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "n")
        .unwrap();
    assert!(n >= 1, "run_now created a job row");

    // Unknown job → 404.
    let (status, _body) = crate::common::post_json_with_token(
        &app,
        "/api/schedules/no_such_service/run_now",
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 404, "run_now of an unregistered job is a 404");
    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn list_reflects_running_for_in_flight_job() {
    let (db, app, token) = setup().await;
    insert_schedule(&db, JOB, 3600).await;
    insert_schedule(&db, "alarm_sweep", 60).await;

    // A non-terminal job of JOB is in flight; alarm_sweep has none.
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status, category) \
             VALUES ('{}', '{JOB}', 'running', 'maintenance')",
            Uuid::new_v4()
        ),
    )
    .await;

    let (status, body) =
        crate::common::get_json_with_token(&app, "/api/schedules", &token).await;
    assert_eq!(status, 200);
    let rows = body.as_array().expect("list is an array");

    let find = |name: &str| {
        rows.iter()
            .find(|r| r["job_name"] == name)
            .unwrap_or_else(|| panic!("missing schedule {name}"))
            .clone()
    };
    // Ordered by job_name.
    assert!(
        rows.windows(2).all(|w| w[0]["job_name"].as_str() <= w[1]["job_name"].as_str()),
        "list is ordered by job_name"
    );
    assert_eq!(find(JOB)["running"], true, "JOB shows running while a job is in flight");
    assert_eq!(find("alarm_sweep")["running"], false, "alarm_sweep is not running");
    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn get_unknown_schedule_is_404() {
    let (db, app, token) = setup().await;
    let (status, _body) =
        crate::common::get_with_token(&app, "/api/schedules/no_such_service", &token).await;
    assert_eq!(status, 404, "GET of a non-existent schedule is a 404");
    crate::common::cleanup_test_db(&db).await;
}
