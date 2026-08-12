//! The three onboarding journeys, each from an empty database to served data.
//!
//! Scenario: a user onboards data by one of the three routes this system supports, a CSV dump, a
//! sync service's repeated ingest cycles, or grab entry through a tool.
//! Expected behaviour: each route provisions its own entities, lands its readings, and serves them
//! back, without any of the three touching another's data.
//!
//! These run as real Keycloak users so they assert what a person can do, not merely that a route
//! accepts a bearer. They self-skip when Keycloak is unreachable unless `REQUIRE_KEYCLOAK` is set,
//! which CI sets so a missing Keycloak fails instead of quietly passing.

use serial_test::serial;

use crate::common::keycloak as kc;
use crate::common::tracks::{self, BAND_CSV, BAND_FLOW, BAND_GRAB, FLOW_CYCLES, FLOW_READINGS_PER_CYCLE};

async fn count(db: &sea_orm::DatabaseConnection, sql: &str) -> i64 {
    use sea_orm::{ConnectionTrait, Statement};
    db.query_one(Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.to_string()))
        .await
        .expect("query")
        .expect("row")
        .try_get::<i64>("", "c")
        .expect("c")
}

#[tokio::test]
#[serial]
async fn csv_dump_onboarding_from_scratch_to_served_readings() {
    if !kc::require_keycloak_or_skip("csv_dump_onboarding").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_csv_track(&app, &jwt).await;
    let codes: Vec<&str> = track.parameters.iter().map(|(c, _)| c.as_str()).collect();
    let csv = tracks::csv_body(&codes, 12, "2025-06-01");

    let (status, preview) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": track.site_id, "csv": csv, "dry_run": true }),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "dry run ({status}): {preview}");
    assert!(
        preview["session_id"].as_str().is_some(),
        "a dry run stages a session for reuse: {preview}"
    );

    let (status, imported) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": track.site_id, "csv": csv, "dry_run": false }),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {imported}");
    assert_eq!(
        imported["unmapped_columns"].as_array().map(Vec::len),
        Some(0),
        "every column resolves to a catalog parameter by code: {imported}"
    );
    assert_eq!(
        imported["mapped_columns"].as_object().map(|m| m.len()),
        Some(2),
        "both parameter columns are ingested: {imported}"
    );
    assert_eq!(imported["row_count"], 12, "twelve data rows parsed: {imported}");

    // Import stages rows into csv_import_staging and a tracked job moves them into readings, so
    // the rows are not visible on the import response alone.
    assert!(
        crate::common::e2e::wait_for_jobs_by_trigger(&db, "csv_import", 30).await,
        "the csv_import job runs and succeeds"
    );

    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings WHERE site_id = '{}' AND site_id IS NOT NULL",
                track.site_id
            )
        )
        .await,
        24,
        "12 rows across 2 parameters land attributed to the site"
    );

    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM data_streams WHERE source_system = 'api' \
                 AND source_key LIKE '{}:%'",
                track.site_id
            )
        )
        .await,
        2,
        "CSV import creates one api-sourced stream per slot"
    );

    let (status, served) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/sites/{}/readings?start=2025-06-01T00:00:00Z&end=2025-06-01T03:00:00Z",
            track.site_id
        ),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "served readings ({status}): {served}");
    let values = crate::common::e2e::values_for(&served, track.parameter_id("TrkCsvDepth"));
    assert_eq!(values.len(), 12, "every imported row is served back: {served}");
    assert!(
        values.iter().all(|v| *v >= BAND_CSV.0 && *v < BAND_CSV.1),
        "served values stay inside the CSV track's band: {values:?}"
    );
}

#[tokio::test]
#[serial]
async fn sensor_flow_onboarding_across_repeated_ingest_cycles() {
    if !kc::require_keycloak_or_skip("sensor_flow_onboarding").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_sensor_flow_track(&app, &jwt).await;
    let stream_id = track.stream_ids[0].clone();
    let sp_id = track.site_parameter_ids[0].clone();

    let ingest = |cycle: usize, jwt: String, app: axum::Router, stream: String| async move {
        let (status, body) = crate::common::post_json_parse_with_token(
            &app,
            "/api/ingest",
            &serde_json::json!({ "stream_id": stream, "readings": tracks::flow_cycle_readings(cycle) }),
            &jwt,
        )
        .await;
        assert_eq!(status, 200, "ingest cycle {cycle} ({status}): {body}");
        body
    };

    let first = ingest(0, jwt.clone(), app.clone(), stream_id.clone()).await;
    assert_eq!(
        first["paired"], false,
        "the first cycle arrives before pairing: {first}"
    );
    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM readings WHERE stream_id = '{stream_id}' AND site_id IS NULL")).await,
        FLOW_READINGS_PER_CYCLE as i64,
        "unpaired readings are stored but unattributed"
    );

    let (status, paired) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &serde_json::json!({ "site_parameter_id": sp_id }),
        &jwt,
    )
    .await;
    assert!((200..300).contains(&status), "pair ({status}): {paired}");

    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM readings WHERE stream_id = '{stream_id}' AND site_id IS NULL")).await,
        0,
        "pairing backfills the cycle that arrived before it"
    );

    for cycle in 1..FLOW_CYCLES {
        ingest(cycle, jwt.clone(), app.clone(), stream_id.clone()).await;
    }

    let total = (FLOW_CYCLES * FLOW_READINGS_PER_CYCLE) as i64;
    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM readings WHERE stream_id = '{stream_id}'")).await,
        total,
        "every cycle landed"
    );
    for (col, why) in [
        ("site_id", "every reading is attributed to the site"),
        (
            "sensor_id",
            "the deployment owning the slot supplies sensor_id to readings ingested through it",
        ),
        (
            "deployment_id",
            "and the deployment itself is stamped, so calibration can be window-resolved later",
        ),
    ] {
        let n = count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings WHERE stream_id = '{stream_id}' AND {col} IS NOT NULL"
            ),
        )
        .await;
        assert_eq!(n, total, "{why} ({col} set on {n} of {total})");
    }
}

#[tokio::test]
#[serial]
async fn grab_and_tool_onboarding_produces_spot_series_with_sample_statistics() {
    if !kc::require_keycloak_or_skip("grab_onboarding").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_grab_track(&app, &jwt).await;
    let parameter_id = track.parameter_id("TrkGrabDoc").to_string();

    // Three replicates whose mean and sample standard deviation are exact: 310, 320, 330 -> 320, 10.
    let replicates = [310.0, 320.0, 330.0];
    let (status, saved) = crate::common::post_json_parse_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": track.site_id,
            "label": "track C",
            "readings": tracks::grab_replicates(&parameter_id, "2025-06-03T09:00:00Z", &replicates),
        }),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "grab samples ({status}): {saved}");
    assert_eq!(saved["inserted"], 3, "one reading per replicate: {saved}");
    assert_eq!(saved["samples_created"], 1, "one sample row: {saved}");

    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings WHERE site_id = '{}' AND measurement_type = 'spot'",
                track.site_id
            )
        )
        .await,
        3,
        "grab readings are classified as spot"
    );

    let row = {
        use sea_orm::{ConnectionTrait, Statement};
        db.query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT mean, stdev, n FROM samples WHERE site_id = '{}' AND parameter_id = '{}'",
                track.site_id, parameter_id
            ),
        ))
        .await
        .expect("query samples")
        .expect("a sample row exists")
    };
    let mean: f64 = row.try_get("", "mean").expect("mean");
    let stdev: f64 = row.try_get("", "stdev").expect("stdev");
    let n: i32 = row.try_get("", "n").expect("n");
    assert_eq!(n, 3, "three replicates counted");
    assert!((mean - 320.0).abs() < 1e-9, "mean of 310/320/330 is 320, got {mean}");
    assert!((stdev - 10.0).abs() < 1e-9, "sample sd of 310/320/330 is 10, got {stdev}");

    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM data_streams WHERE source_system = 'grab_sample' \
                 AND source_key = '{}:{}'",
                track.site_id, parameter_id
            )
        )
        .await,
        1,
        "grab entry creates its own grab_sample stream for the slot"
    );
}

#[tokio::test]
#[serial]
async fn the_three_tracks_share_no_entities_or_readings() {
    if !kc::require_keycloak_or_skip("track_disjointness").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let a = tracks::onboard_csv_track(&app, &jwt).await;
    let b = tracks::onboard_sensor_flow_track(&app, &jwt).await;
    let c = tracks::onboard_grab_track(&app, &jwt).await;

    let sites = [&a.site_id, &b.site_id, &c.site_id];
    let projects = [&a.project_id, &b.project_id, &c.project_id];
    for (label, ids) in [("site", sites), ("project", projects)] {
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), 3, "the three tracks hold distinct {label} ids");
    }

    let params: Vec<&str> = a
        .parameters
        .iter()
        .chain(b.parameters.iter())
        .chain(c.parameters.iter())
        .map(|(_, id)| id.as_str())
        .collect();
    let unique: std::collections::HashSet<_> = params.iter().collect();
    assert_eq!(unique.len(), params.len(), "no parameter is shared between tracks");

    assert_ne!(a.sensor_id, b.sensor_id, "tracks A and B differ on sensor");
    assert_ne!(b.sensor_id, c.sensor_id, "tracks B and C hold different sensors");
    assert!(a.sensor_id.is_none(), "the CSV track deliberately has no sensor");

    for (label, band) in [("csv", BAND_CSV), ("flow", BAND_FLOW), ("grab", BAND_GRAB)] {
        assert!(band.0 < band.1, "{label} band is ordered");
    }
    let bands = [BAND_CSV, BAND_FLOW, BAND_GRAB];
    for i in 0..bands.len() {
        for j in (i + 1)..bands.len() {
            assert!(
                bands[i].1 <= bands[j].0 || bands[j].1 <= bands[i].0,
                "value bands {i} and {j} must not overlap, or a leaked reading would pass unnoticed"
            );
        }
    }
}
