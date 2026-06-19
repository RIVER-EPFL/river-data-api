//! CSV import runs on the worker pool: the handler stages the parsed rows and enqueues a
//! `csv_import` job; a worker claims it, inserts the readings, recomputes derived values, and the
//! staging rows are deleted. This is the durability flip — no inline `spawn_tracked_job_ctx` whose
//! in-memory `Vec` would strand on a dead replica.
//!
//! Run with: cargo test --test readings

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

const CSV: &str = "DateTime,Dissolved_O2,DO_Temperature\n\
2025-06-01 00:00:00,250,12.0\n\
2025-06-01 00:10:00,300,12.5\n";

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one(Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.to_string()))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "n")
        .unwrap()
}

async fn poll_count(db: &DatabaseConnection, sql: &str, want: i64, max_secs: u64) -> i64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(max_secs);
    loop {
        let n = scalar_i64(db, sql).await;
        if n == want || std::time::Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

#[tokio::test]
#[serial]
async fn csv_import_runs_on_worker_and_clears_staging() {
    let (db, app, token) = setup().await;

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": crate::common::SITE1_ID, "csv": CSV }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");
    let job_id = resp["derived_job_id"].as_str().expect("a worker job id is returned");

    // The handler stages the rows; the worker reads them back. (Two parameters x two rows = 4 rows.)
    // It races with the background worker, so just assert the readings land and staging is drained.
    let readings = poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings WHERE site_id = '{}' AND time >= '2025-06-01T00:00:00Z'",
            crate::common::SITE1_ID
        ),
        4,
        10,
    )
    .await;
    assert_eq!(readings, 4, "all four staged readings should be inserted by the worker");

    let staging_left = poll_count(
        &db,
        "SELECT count(*) AS n FROM csv_import_staging",
        0,
        10,
    )
    .await;
    assert_eq!(staging_left, 0, "the worker deletes its staged rows on completion");

    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT status FROM reprocessing_jobs WHERE id = '{job_id}'"),
        ))
        .await
        .unwrap()
        .unwrap();
    let job_status: String = row.try_get("", "status").unwrap();
    assert_eq!(job_status, "completed", "the csv_import job reaches completed");
}

#[tokio::test]
#[serial]
async fn csv_import_recomputes_derived_via_worker() {
    let (db, app, token) = setup().await;

    // A derived parameter (DO mg/L = Dissolved_O2 * 0.032) assigned to the site, so the imported
    // Dissolved_O2 rows produce derived readings when the worker recomputes them.
    let derived_name = format!("DOmgL_{}", Uuid::new_v4().simple());
    let (_s, def) = crate::common::post_json_parse_with_token(
        &app,
        "/api/derived_parameters",
        &serde_json::json!({
            "code": derived_name, "name": "DO mg/L", "units": "mg/L",
            "formula": "Dissolved_O2 * 0.032",
        }),
        &token,
    )
    .await;
    let derived_def_id = def["id"].as_str().unwrap().to_string();
    let output_parameter_id = def["output_parameter_id"].as_str().unwrap().to_string();
    crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID, "parameter_id": output_parameter_id, "name": derived_name,
            "sensor_type": "derived", "is_derived": true, "derived_definition_id": derived_def_id,
            "display_units": "mg/L",
        }),
        &token,
    )
    .await;

    let (status, _resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": crate::common::SITE1_ID, "csv": CSV }),
        &token,
    )
    .await;
    assert_eq!(status, 200);

    // The worker's Phase 2 recompute writes the derived parameter's readings.
    let derived = poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE parameter_id = '{output_parameter_id}' AND time >= '2025-06-01T00:00:00Z'"
        ),
        2,
        10,
    )
    .await;
    assert_eq!(derived, 2, "the worker recomputes a derived value per imported timestamp");
}
