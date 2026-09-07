//! `POST /readings/import_csv` with `tool`: naming a standard curve per curve slot.
//!
//! Scenario: a lab sheet of analyser replicates is imported as tool entry and the operator names
//! the standard curve each plate was read against, once for the file or per row in a column.
//! Expected behaviour: the plan reports where each slot's curve comes from, a slot or curve the
//! tool or catalog does not have refuses the request, and a row's bad cell is that row's error.
//!
//! Run: cargo test --test readings csv_import_tool_curves -- --test-threads=1

use sea_orm::DatabaseConnection;
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::SITE1_ID;
use crate::common::sensor_lifecycle::create_sensor_without_curve;

struct Fixture {
    app: axum::Router,
    db: DatabaseConnection,
    token: String,
}

async fn setup() -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    // The catalog parameter the doc tool's replicates are readings of, under the code its manifest
    // names.
    crate::common::exec(
        &db,
        "INSERT INTO parameters (id, code, name, category) \
         VALUES ('00000000-0000-4000-b000-0000000000d0', 'DOC_ppb', 'DOC', 'measurement')",
    )
    .await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    Fixture { app, db, token }
}

async fn create_curve(fx: &Fixture, name: &str) -> Uuid {
    let sensor_id = create_sensor_without_curve(&fx.db, "TOC analyser").await;
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/standard_curves",
        &json!({ "sensor_id": sensor_id, "name": name, "slope": 2.0, "intercept": 1.0 }),
        &fx.token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "creating curve ({status}): {body}"
    );
    body["id"].as_str().unwrap().parse().unwrap()
}

async fn plan(fx: &Fixture, body: serde_json::Value) -> (u16, serde_json::Value) {
    let mut body = body;
    body["site"] = json!(SITE1_ID);
    body["dry_run"] = json!(true);
    crate::common::post_json_parse_with_token(&fx.app, "/api/readings/import_csv", &body, &fx.token)
        .await
}

#[tokio::test]
#[serial]
async fn curves_without_a_tool_are_refused() {
    let fx = setup().await;
    let (status, body) = plan(
        &fx,
        json!({
            "csv": "DateTime,DOC\n2025-06-01 10:00:00,120\n",
            "curves": { "std_curve": Uuid::new_v4() },
        }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.to_string().contains("tool"), "{body}");
}

#[tokio::test]
#[serial]
async fn a_slot_the_tool_does_not_declare_is_refused_naming_its_slots() {
    let fx = setup().await;
    let curve = create_curve(&fx, "Plate A").await;
    let (status, body) = plan(
        &fx,
        json!({
            "csv": "DateTime,DOC_rep_1\n2025-06-01 10:00:00,120\n",
            "tool": "doc",
            "curves": { "plate": curve },
        }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let msg = body.to_string();
    assert!(
        msg.contains("'plate'") && msg.contains("std_curve"),
        "{msg}"
    );
}

#[tokio::test]
#[serial]
async fn a_request_curve_nothing_carries_is_refused() {
    let fx = setup().await;
    let missing = Uuid::new_v4();
    let (status, body) = plan(
        &fx,
        json!({
            "csv": "DateTime,DOC_rep_1\n2025-06-01 10:00:00,120\n",
            "tool": "doc",
            "curves": { "std_curve": missing },
        }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.to_string().contains(&missing.to_string()), "{body}");
}

#[tokio::test]
#[serial]
async fn the_plan_reports_each_slot_and_a_bad_cell_is_the_rows_error() {
    let fx = setup().await;
    let curve = create_curve(&fx, "Plate A").await;
    let other = create_curve(&fx, "Plate B").await;
    let missing = Uuid::new_v4();
    let csv = format!(
        "DateTime,DOC_rep_1,DOC_rep_2,Std_Curve\n\
         2025-06-01 10:00:00,120,125,\n\
         2025-06-02 10:00:00,130,131,{other}\n\
         2025-06-03 10:00:00,140,141,plate 3\n\
         2025-06-04 10:00:00,150,151,{missing}\n"
    );
    let (status, body) = plan(
        &fx,
        json!({ "csv": csv, "tool": "doc", "curves": { "std_curve": curve } }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["curves"].as_array().map(Vec::len), Some(1), "{body}");
    let slot = &body["curves"][0];
    assert_eq!(slot["slot"], "std_curve");
    assert_eq!(slot["column"], "Std_Curve");
    assert_eq!(slot["standard_curve_id"], json!(curve));
    assert_eq!(slot["name"], "Plate A");
    // The curve column is a slot, not an ignored input.
    assert!(
        body["unmapped_columns"].as_array().unwrap().is_empty(),
        "{body}"
    );
    assert_eq!(body["mapped_columns"]["DOC_rep_1"], "DOC");
    // Rows 4 and 5 fail on their own cell; the rest of the file plans.
    assert_eq!(body["row_count"], 2, "{body}");
    assert_eq!(body["error_count"], 2, "{body}");
    let errors = body["errors"].as_array().unwrap();
    assert_eq!(errors[0]["row"], 4);
    assert!(
        errors[0]["message"].as_str().unwrap().contains("plate 3"),
        "{errors:?}"
    );
    assert_eq!(errors[1]["row"], 5);
    assert!(
        errors[1]["message"]
            .as_str()
            .unwrap()
            .contains(&missing.to_string()),
        "{errors:?}"
    );
    assert_eq!(body["tool_runs_created"], 0);
}

#[tokio::test]
#[serial]
async fn a_plain_import_reports_no_slots() {
    let fx = setup().await;
    let (status, body) = plan(
        &fx,
        json!({ "csv": "DateTime,WaterTempdegC\n2025-06-01 10:00:00,12.0\n" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["curves"], json!([]));
}
