//! Public readings measurement_type support: the `measurement_type` filter (same semantics as
//! the private endpoint — 'continuous' includes untagged legacy rows), the
//! `include_measurement_type` per-point annotation (JSON + CSV), and cache-key isolation between
//! filter values (a spot query must never serve a cached continuous payload).
//!
//! Run: cargo test --test public_api -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

async fn exec(db: &DatabaseConnection, sql: &str) {
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("SQL failed: {e}\nQuery: {sql}"));
}

/// Public project + exposed DO_Temperature (continuous seed data), plus one spot grab reading
/// injected into the same parameter so both cadences coexist in the window.
async fn setup_with_spot() -> (DatabaseConnection, axum::Router) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    exec(
        &db,
        &format!(
            "UPDATE projects SET is_public = true, public_code = 'test-river' WHERE id = '{}'",
            crate::common::PROJECT_ID
        ),
    )
    .await;
    exec(
        &db,
        &format!(
            "UPDATE sites SET public_code = 'upstream' WHERE id = '{}'",
            crate::common::SITE1_ID,
        ),
    )
    .await;
    exec(
        &db,
        &format!(
            "UPDATE site_parameters SET is_public = true WHERE id = '{}'",
            crate::common::PARAM_S1_TEMP_ID,
        ),
    )
    .await;

    let stream_id = Uuid::new_v4();
    exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active) \
             VALUES ('{stream_id}', 'grab_sample', '{}', true)",
            Uuid::new_v4()
        ),
    )
    .await;
    // Off the seeded 10-min grid so the spot point is its own timestamp.
    exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                raw_value, calibrated_value, measurement_type) \
             VALUES ('{stream_id}', '{site}', '{param}', '2025-01-15T00:05:30Z', 0, 99.5, 99.5, 'spot')",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await;

    let app = crate::common::build_test_app(db.clone());
    (db, app)
}

const WINDOW: &str = "start=2025-01-15T00:00:00Z&end=2025-01-15T01:00:00Z";

fn first_param(body: &serde_json::Value) -> &serde_json::Value {
    body["parameters"]
        .as_array()
        .and_then(|a| a.first())
        .unwrap_or_else(|| panic!("no parameters in {body}"))
}

#[tokio::test]
#[serial]
async fn filter_and_annotation_with_cache_isolation() {
    let (_db, app) = setup_with_spot().await;
    let base = "/api/public/test-river/sites/upstream/readings";

    // Unfiltered: continuous grid + the injected spot point.
    let (status, all) = crate::common::get_json(&app, &format!("{base}?{WINDOW}")).await;
    assert_eq!(status, 200, "unfiltered: {all}");
    let all_count = all["times"].as_array().unwrap().len();
    assert_eq!(all_count, 8, "7 grid points (00:00..=01:00) + 1 spot: {all}");

    // Spot only.
    let (status, spot) =
        crate::common::get_json(&app, &format!("{base}?{WINDOW}&measurement_type=spot")).await;
    assert_eq!(status, 200, "spot: {spot}");
    assert_eq!(spot["times"].as_array().unwrap().len(), 1, "spot only: {spot}");
    assert_eq!(first_param(&spot)["values"][0], 99.5);

    // Continuous excludes the spot point (and would include untagged legacy rows).
    let (status, cont) = crate::common::get_json(
        &app,
        &format!("{base}?{WINDOW}&measurement_type=continuous"),
    )
    .await;
    assert_eq!(status, 200, "continuous: {cont}");
    assert_eq!(
        cont["times"].as_array().unwrap().len(),
        7,
        "continuous excludes the spot point: {cont}"
    );

    // Cache isolation: re-issue the three queries; each must return its own shape, not another
    // key's cached payload.
    let (_, all2) = crate::common::get_json(&app, &format!("{base}?{WINDOW}")).await;
    assert_eq!(all2["times"].as_array().unwrap().len(), all_count);
    let (_, spot2) =
        crate::common::get_json(&app, &format!("{base}?{WINDOW}&measurement_type=spot")).await;
    assert_eq!(spot2["times"].as_array().unwrap().len(), 1);

    // Annotation: per-point measurement_type array aligned with values; absent without the flag.
    let (status, annotated) = crate::common::get_json(
        &app,
        &format!("{base}?{WINDOW}&include_measurement_type=true"),
    )
    .await;
    assert_eq!(status, 200, "annotated: {annotated}");
    let p = first_param(&annotated);
    let mts = p["measurement_types"].as_array().expect("measurement_types present");
    assert_eq!(mts.len(), annotated["times"].as_array().unwrap().len());
    let spot_count = mts.iter().filter(|m| *m == "spot").count();
    let cont_count = mts.iter().filter(|m| *m == "continuous").count();
    assert_eq!(spot_count, 1, "one spot point annotated: {p}");
    assert_eq!(cont_count, 7, "grid points annotated continuous: {p}");
    assert!(
        first_param(&all)["measurement_types"].is_null(),
        "no annotation without the flag"
    );

    // Invalid filter value → 400.
    let (status, _) = crate::common::get_json(
        &app,
        &format!("{base}?{WINDOW}&measurement_type=grab"),
    )
    .await;
    assert_eq!(status, 400);
}

#[tokio::test]
#[serial]
async fn csv_annotation_adds_measurement_type_column() {
    let (_db, app) = setup_with_spot().await;

    let (status, body) = crate::common::get(
        &app,
        &format!(
            "/api/public/test-river/sites/upstream/readings?{WINDOW}&format=csv&include_measurement_type=true"
        ),
    )
    .await;
    assert_eq!(status, 200, "csv: {body}");
    let header = body.lines().next().unwrap_or_default();
    assert!(
        header.contains("_measurement_type"),
        "CSV header carries the measurement_type column: {header}"
    );
    assert!(body.contains("spot"), "spot row present in CSV: {body}");

    let (status, plain) = crate::common::get(
        &app,
        &format!("/api/public/test-river/sites/upstream/readings?{WINDOW}&format=csv"),
    )
    .await;
    assert_eq!(status, 200);
    let plain_header = plain.lines().next().unwrap_or_default();
    assert!(
        !plain_header.contains("_measurement_type"),
        "no annotation column without the flag: {plain_header}"
    );
}
