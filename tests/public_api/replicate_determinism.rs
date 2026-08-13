//! Public readings must serve one deterministic value for a replicated timestamp: only
//! replicate_index 0 rows are selected and a replicate group's point value is its sample mean,
//! matching the private endpoint.
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

/// Public project with an exposed parameter and a triplicate grab (replicates 0/1/2 behind one
/// sample) at a timestamp off the seeded grid.
async fn setup_with_replicates() -> (DatabaseConnection, axum::Router) {
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
    let sample_id = Uuid::new_v4();
    exec(
        &db,
        &format!(
            "INSERT INTO samples (id, site_id, parameter_id, collected_at) \
             VALUES ('{sample_id}', '{}', '{}', '2025-01-15T00:05:30Z')",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    for (idx, value) in [(0, 10.0), (1, 20.0), (2, 30.0)] {
        exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                    raw_value, calibrated_value, measurement_type, sample_id) \
                 VALUES ('{stream_id}', '{site}', '{param}', '2025-01-15T00:05:30Z', {idx}, \
                         {value}, {value}, 'spot', '{sample_id}')",
                site = crate::common::SITE1_ID,
                param = crate::common::GLOBAL_PARAM_TEMP_ID,
            ),
        )
        .await;
    }

    let app = crate::common::build_test_app(db.clone());
    (db, app)
}

const WINDOW: &str = "start=2025-01-15T00:00:00Z&end=2025-01-15T01:00:00Z";

#[tokio::test]
#[serial]
async fn replicated_timestamp_serves_one_value_the_sample_mean() {
    let (_db, app) = setup_with_replicates().await;
    let base = "/api/public/test-river/sites/upstream/readings";

    let (status, spot) =
        crate::common::get_json(&app, &format!("{base}?{WINDOW}&measurement_type=spot")).await;
    assert_eq!(status, 200, "spot: {spot}");
    let times = spot["times"].as_array().unwrap();
    assert_eq!(
        times.len(),
        1,
        "one point for the replicate group, not three: {spot}"
    );
    let values = spot["parameters"][0]["values"].as_array().unwrap();
    assert_eq!(values.len(), 1, "{spot}");
    assert!(
        (values[0].as_f64().unwrap() - 20.0).abs() < 1e-9,
        "the served value is the sample mean: {values:?}"
    );

    let (status, all) = crate::common::get_json(&app, &format!("{base}?{WINDOW}")).await;
    assert_eq!(status, 200, "unfiltered: {all}");
    assert_eq!(
        all["times"].as_array().unwrap().len(),
        8,
        "7 grid points + 1 grab point, replicates do not multiply timestamps: {all}"
    );
}
