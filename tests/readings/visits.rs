//! The visits table: one row per collection event with the wide per-parameter cells, and the
//! per-event detail grid behind a row.
//!
//! Run: cargo test --test readings visits -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::{GLOBAL_PARAM_DO_ID, GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const T1: &str = "2025-06-01T08:00:00Z";
const T2: &str = "2025-06-08T09:30:00Z";

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

async fn save_two_visits(app: &axum::Router, token: &str) {
    let (status, body) = crate::common::post_json_with_token(
        app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "readings": [
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 10.0, "time": T1 },
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 12.0, "time": T1 },
                { "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 4.2, "time": T1 },
            ],
        }),
        token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = crate::common::post_json_with_token(
        app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "readings": [{ "parameter_id": GLOBAL_PARAM_DO_ID, "value": 9.0, "time": T2 }],
        }),
        token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

fn cell<'a>(row: &'a serde_json::Value, parameter_id: &str) -> Option<&'a serde_json::Value> {
    row["cells"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["parameter_id"] == parameter_id)
}

#[tokio::test]
#[serial]
async fn the_list_is_the_wide_portal_row() {
    let (_db, app, token) = setup().await;
    save_two_visits(&app, &token).await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{SITE1_ID}/visits"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 2);
    let columns = body["expected_parameters"].as_array().unwrap();
    assert!(
        columns.iter().any(|c| c["parameter_id"] == GLOBAL_PARAM_DO_ID)
            && columns.iter().any(|c| c["parameter_id"] == GLOBAL_PARAM_TEMP_ID),
        "both spot parameters are grid columns: {body}"
    );

    let visits = body["visits"].as_array().unwrap();
    assert_eq!(visits[0]["collected_at"], T2, "newest first");
    assert_eq!(visits[0]["parameters_filled"], 1);
    assert_eq!(visits[1]["parameters_filled"], 2);
    let do_cell = cell(&visits[1], GLOBAL_PARAM_DO_ID).expect("DO cell at T1");
    assert_eq!(do_cell["value"], 11.0, "the served value is the sample mean");
    let temp_cell = cell(&visits[1], GLOBAL_PARAM_TEMP_ID).expect("Temp cell at T1");
    assert_eq!(temp_cell["value"], 4.2);
    assert!(cell(&visits[0], GLOBAL_PARAM_TEMP_ID).is_none(), "no Temp at T2");
}

#[tokio::test]
#[serial]
async fn the_detail_grid_shows_replicates_and_sample_stats() {
    let (db, app, token) = setup().await;
    save_two_visits(&app, &token).await;

    let event_id: String = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT id::text AS id FROM collection_events \
                 WHERE site_id = '{SITE1_ID}' AND collected_at = '{T1}'"
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "id")
        .unwrap();

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/collection_events/{event_id}/detail"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["collected_at"], T1);
    let cells = body["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 2);
    let do_cell = cells
        .iter()
        .find(|c| c["parameter_id"] == GLOBAL_PARAM_DO_ID)
        .unwrap();
    assert_eq!(do_cell["served_value"], 11.0);
    assert_eq!(do_cell["sample"]["n"], 2);
    assert_eq!(do_cell["replicates"].as_array().unwrap().len(), 2);
    assert_eq!(do_cell["sample"]["has_provenance"], false);
}

#[tokio::test]
#[serial]
async fn a_finding_marks_its_cell_and_the_row() {
    let (db, app, token) = setup().await;
    save_two_visits(&app, &token).await;

    db.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "INSERT INTO replicate_audit_holds \
                 (kind, site_id, parameter_id, group_time, tool, expected, computed, delta, status) \
             VALUES ('stale_output', '{SITE1_ID}', '{GLOBAL_PARAM_DO_ID}', '{T1}', 'chain_b', \
                     '{{}}', '{{}}', '{{}}', 'pending')"
        ),
    ))
    .await
    .unwrap();

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{SITE1_ID}/visits"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let visits = body["visits"].as_array().unwrap();
    assert_eq!(visits[1]["findings_open"], 1);
    assert_eq!(
        cell(&visits[1], GLOBAL_PARAM_DO_ID).unwrap()["finding"],
        "stale_output"
    );
    assert_eq!(visits[0]["findings_open"], 0);
}

#[tokio::test]
#[serial]
async fn a_fully_withdrawn_group_empties_its_cell() {
    let (db, app, token) = setup().await;
    save_two_visits(&app, &token).await;

    db.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "UPDATE readings SET withdrawn_at = NOW() \
             WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}' \
               AND time = '{T2}'"
        ),
    ))
    .await
    .unwrap();

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{SITE1_ID}/visits"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let visits = body["visits"].as_array().unwrap();
    assert_eq!(visits[0]["parameters_filled"], 0, "a withdrawn group no longer fills");
    let do_cell = cell(&visits[0], GLOBAL_PARAM_DO_ID).unwrap();
    assert_eq!(do_cell["withdrawn"], true);
}
