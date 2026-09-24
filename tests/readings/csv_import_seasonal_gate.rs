//! The seasonal Check gate has a CSV arm: a spot file is screened row by row in `dry_run`, and
//! the commit is held to the values that were screened.
//!
//! Run: cargo test --test readings csv_import_seasonal_gate -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::{GLOBAL_PARAM_DO_ID, SITE1_ID};

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

async fn seed_visit(app: &axum::Router, token: &str, at: &str, values: &[f64]) {
    let readings: Vec<serde_json::Value> = values
        .iter()
        .map(|v| json!({ "parameter_id": GLOBAL_PARAM_DO_ID, "value": v, "time": at }))
        .collect();
    let (status, body) = crate::common::post_json_with_token(
        app,
        "/api/grab_samples",
        &json!({ "site_id": SITE1_ID, "readings": readings }),
        token,
    )
    .await;
    assert_eq!(status, 200, "seed visit at {at}: {body}");
}

async fn seed_june_history(app: &axum::Router, token: &str) {
    seed_visit(app, token, "2021-06-10T10:00:00Z", &[9.0, 10.0, 11.0]).await;
    seed_visit(app, token, "2022-05-20T10:00:00Z", &[10.0, 12.0]).await;
    seed_visit(app, token, "2023-07-05T10:00:00Z", &[8.0, 9.5]).await;
}

async fn import(
    app: &axum::Router,
    token: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(app, "/api/readings/import_csv", &body, token).await
}

const OUTLIER_CSV: &str = "DateTime,Dissolved_O2\n\
2025-06-15 10:00:00,10.5\n\
2025-06-16 10:00:00,25000\n";

const CLEAN_CSV: &str = "DateTime,Dissolved_O2\n\
2025-06-15 10:00:00,10.5\n\
2025-06-16 10:00:00,9.8\n";

#[tokio::test]
#[serial]
async fn a_spot_import_is_screened_in_dry_run_and_the_commit_is_held_to_the_check() {
    let (_db, app, token) = setup().await;
    seed_june_history(&app, &token).await;

    let (status, plan) = import(
        &app,
        &token,
        json!({ "site": SITE1_ID, "csv": OUTLIER_CSV, "measurement_type": "spot", "dry_run": true }),
    )
    .await;
    assert_eq!(status, 200, "{plan}");
    let check = &plan["check"];
    assert_eq!(check["screened"], 2, "{plan}");
    assert_eq!(check["warnings"], 1, "{plan}");
    assert_eq!(check["findings"][0]["row"], 3, "{plan}");
    assert_eq!(check["findings"][0]["class"], "above_max", "{plan}");
    assert_eq!(check["method"]["window_months"], 2, "{plan}");
    let check_id = check["check_id"].as_str().expect("check id").to_string();

    // Committing an outlier without naming the check is refused, with the findings.
    let (status, body) = import(
        &app,
        &token,
        json!({ "site": SITE1_ID, "csv": OUTLIER_CSV, "measurement_type": "spot" }),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["detail"]["findings"][0]["row"], 3, "{body}");

    // A check over a different file cannot vouch for this one.
    let (status, body) = import(
        &app,
        &token,
        json!({ "site": SITE1_ID, "csv": CLEAN_CSV, "measurement_type": "spot", "check_id": check_id }),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("fresh check"),
        "{body}"
    );

    // The screened file commits under its check.
    let (status, body) = import(
        &app,
        &token,
        json!({ "site": SITE1_ID, "csv": OUTLIER_CSV, "measurement_type": "spot", "check_id": check_id }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["inserted_total"], 2, "{body}");
    assert_eq!(body["check"]["check_id"], check_id, "{body}");
}

#[tokio::test]
#[serial]
async fn a_clean_spot_file_needs_a_check_and_a_continuous_file_is_not_screened() {
    let (_db, app, token) = setup().await;
    seed_june_history(&app, &token).await;

    let (status, body) = import(
        &app,
        &token,
        json!({ "site": SITE1_ID, "csv": CLEAN_CSV, "measurement_type": "spot" }),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["detail"]["warnings"], 0, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("dry_run"),
        "{body}"
    );

    let (status, plan) = import(
        &app,
        &token,
        json!({ "site": SITE1_ID, "csv": CLEAN_CSV, "measurement_type": "spot", "dry_run": true }),
    )
    .await;
    assert_eq!(status, 200, "{plan}");
    let check_id = plan["check"]["check_id"]
        .as_str()
        .expect("check id")
        .to_string();
    let (status, body) = import(
        &app,
        &token,
        json!({ "site": SITE1_ID, "csv": CLEAN_CSV, "measurement_type": "spot", "check_id": check_id }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["inserted_total"], 2, "{body}");
    assert_eq!(body["check"]["check_id"], check_id, "{body}");

    let (status, body) = import(
        &app,
        &token,
        json!({ "site": SITE1_ID, "csv": OUTLIER_CSV, "measurement_type": "continuous" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body["check"].is_null(), "{body}");
}

const CALCULATION: &str = "screened_output_probe";
const OUTPUT_CODE: &str = "screened_ratio";

/// A formula calculation publishing twice the temperature, with its output declared at the site
/// and a June history there. Formula engine only, so no R runs.
async fn seed_calculation(db: &DatabaseConnection, app: &axum::Router, token: &str) {
    for sql in [
        format!("UPDATE tool_scripts SET active_version_id = NULL WHERE name = '{CALCULATION}'"),
        format!("DELETE FROM tool_scripts WHERE name = '{CALCULATION}'"),
        format!(
            "INSERT INTO tool_scripts (name, label, engine, created_by) \
             VALUES ('{CALCULATION}', 'Screened output probe', 'formula', 'test')"
        ),
    ] {
        crate::common::exec(db, &sql).await;
    }
    let script_id: String = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT id::text AS id FROM tool_scripts WHERE name = '{CALCULATION}'"),
        ))
        .await
        .expect("query")
        .expect("the calculation")
        .try_get("", "id")
        .expect("id");
    let (status, text) = crate::common::save_formula_set(
        app,
        token,
        &script_id,
        json!([{
            "code": OUTPUT_CODE,
            "name": OUTPUT_CODE,
            "units": "ratio",
            "formula": "DO_Temperature * 2",
            "ordinal": 1,
        }]),
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "save the formula ({status}): {text}"
    );
    let output_id: String = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT id::text AS id FROM parameters WHERE LOWER(code) = LOWER('{OUTPUT_CODE}')"
            ),
        ))
        .await
        .expect("query")
        .expect("the formula save minted its output")
        .try_get("", "id")
        .expect("id");
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type, is_active, cadence) \
             VALUES (gen_random_uuid(), '{SITE1_ID}', '{output_id}', '{OUTPUT_CODE}', 'lab', true, 'low')"
        ),
    )
    .await;
    for (at, values) in [
        ("2021-06-10T10:00:00Z", [20.0, 22.0]),
        ("2022-05-20T10:00:00Z", [21.0, 23.0]),
        ("2023-07-05T10:00:00Z", [19.0, 24.0]),
    ] {
        let readings: Vec<serde_json::Value> = values
            .iter()
            .map(|v| json!({ "parameter_id": output_id, "value": v, "time": at }))
            .collect();
        let (status, body) = crate::common::post_json_with_token(
            app,
            "/api/grab_samples",
            &json!({ "site_id": SITE1_ID, "readings": readings }),
            token,
        )
        .await;
        assert_eq!(status, 200, "seed output history at {at}: {body}");
    }
}

/// Twice 11 is inside the output's June range; twice 5000 is not, though nothing screens the
/// temperature itself.
const TOOL_OUTLIER_CSV: &str = "DateTime,DO_Temperature\n\
2025-06-15 10:00:00,11\n\
2025-06-16 10:00:00,5000\n";

/// Scenario: a tool-entry file whose inputs are unremarkable and one of whose computed outputs is
/// far outside the site's seasonal range for that output.
///
/// Expected behaviour: the dry run screens the output and reports the row, a commit naming no
/// check is refused, and the commit under the dry run's check stores both rows.
#[tokio::test]
#[serial]
async fn a_tool_entry_import_screens_the_outputs_it_publishes() {
    let (db, app, token) = setup().await;
    seed_calculation(&db, &app, &token).await;

    let (status, plan) = import(
        &app,
        &token,
        json!({ "site": SITE1_ID, "csv": TOOL_OUTLIER_CSV, "tool": CALCULATION, "dry_run": true }),
    )
    .await;
    assert_eq!(status, 200, "{plan}");
    assert_eq!(
        plan["tool_runs_created"], 0,
        "a dry run stores no run: {plan}"
    );
    let check = &plan["check"];
    assert_eq!(check["warnings"], 1, "{plan}");
    assert_eq!(check["findings"][0]["row"], 3, "{plan}");
    assert_eq!(check["findings"][0]["class"], "above_max", "{plan}");
    let check_id = check["check_id"].as_str().expect("check id").to_string();

    let (status, body) = import(
        &app,
        &token,
        json!({ "site": SITE1_ID, "csv": TOOL_OUTLIER_CSV, "tool": CALCULATION }),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["detail"]["findings"][0]["row"], 3, "{body}");

    let (status, body) = import(
        &app,
        &token,
        json!({ "site": SITE1_ID, "csv": TOOL_OUTLIER_CSV, "tool": CALCULATION, "check_id": check_id }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["error_count"], 0, "{body}");
    assert_eq!(body["inserted_total"], 2, "{body}");
    assert_eq!(body["check"]["check_id"], check_id, "{body}");
}
