//! The apply-time entity resolution the pairing plan performs: an existing site is reused by
//! case-insensitive name, an existing parameter by alias, a new site takes its coordinates from
//! the stream metadata, and an existing site missing them has them backfilled.
//!
//! These are single-domain rules over the plan routes rather than a story crossing subsystems,
//! which is why they sit here and not in `tests/e2e/pairing_plan_lifecycle.rs`.
//!
//! Run: cargo test --test sync pairing_plan_resolution -- --test-threads=1

use serial_test::serial;

use crate::common::e2e::count;
use crate::common::plans::run_plan_action;

#[tokio::test]
#[serial]
async fn apply_reuses_existing_site_by_case_insensitive_name() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let project_id = uuid::Uuid::new_v4().to_string();
    let site_id = uuid::Uuid::new_v4().to_string();
    crate::common::exec(
        &db,
        &format!("INSERT INTO projects (id, name) VALUES ('{project_id}', 'METALP')"),
    )
    .await;
    crate::common::exec(&db, &format!(
        "INSERT INTO sites (id, project_id, name, latitude, longitude) VALUES ('{site_id}', '{project_id}', 'gl1_dn', 46.0, 7.0)"
    )).await;

    let stream = uuid::Uuid::new_v4().to_string();
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &stream,
        "metalp",
        "k1",
        "METALP",
        "GL1_DN",
        "Conductivity",
        "uS/cm",
        None,
        0,
    )
    .await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({"source_system": "metalp"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create ({status}): {plan}");
    let plan_id = plan["id"].as_str().unwrap().to_string();

    let counts = run_plan_action(&app, &token, &plan_id, "apply").await;
    assert_eq!(
        counts["sites_created"], 0,
        "existing site reused by case-insensitive name"
    );
    assert_eq!(counts["projects_created"], 0, "existing project reused");

    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM data_streams ds JOIN site_parameters sp ON ds.site_parameter_id = sp.id \
             WHERE ds.id = '{stream}' AND sp.site_id = '{site_id}'"
        )).await,
        1, "stream landed on the pre-existing site"
    );
}

#[tokio::test]
#[serial]
async fn apply_reuses_existing_parameter_via_alias() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    // code differs (so the LOWER(code) match misses) but the hierarchy name is in aliases.
    let param_id = uuid::Uuid::new_v4().to_string();
    crate::common::exec(&db, &format!(
        "INSERT INTO parameters (id, code, name, default_units, category, aliases) \
         VALUES ('{param_id}', 'cond', 'Cond Display', 'uS/cm', 'measurement', ARRAY['Conductivity']::text[])"
    )).await;

    let stream = uuid::Uuid::new_v4().to_string();
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &stream,
        "metalp",
        "k1",
        "METALP",
        "GL1_DN",
        "Conductivity",
        "uS/cm",
        None,
        0,
    )
    .await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({"source_system": "metalp"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create ({status}): {plan}");
    let plan_id = plan["id"].as_str().unwrap().to_string();

    let counts = run_plan_action(&app, &token, &plan_id, "apply").await;
    assert_eq!(
        counts["parameters_created"], 0,
        "parameter reused via alias match"
    );
    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM data_streams ds JOIN site_parameters sp ON ds.site_parameter_id = sp.id \
             WHERE ds.id = '{stream}' AND sp.parameter_id = '{param_id}'"
        )).await,
        1, "stream resolved to the aliased parameter"
    );
}

#[tokio::test]
#[serial]
async fn apply_creates_new_site_with_metadata_coordinates() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let stream = uuid::Uuid::new_v4().to_string();
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &stream,
        "metalp",
        "k1",
        "METALP",
        "NewSite",
        "Conductivity",
        "uS/cm",
        Some((46.25, 7.75, 2100.0)),
        0,
    )
    .await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({"source_system": "metalp"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create ({status}): {plan}");
    let plan_id = plan["id"].as_str().unwrap().to_string();
    run_plan_action(&app, &token, &plan_id, "apply").await;

    assert_eq!(
        count(&db, "SELECT count(*) AS c FROM sites WHERE LOWER(name) = 'newsite' AND latitude = 46.25 AND altitude_m = 2100.0").await,
        1, "newly created site carries the stream metadata coordinates"
    );
}

#[tokio::test]
#[serial]
async fn apply_backfills_coordinates_onto_existing_site_lacking_them() {
    // USER_STORIES API-behaviour: "Site coordinates from stream metadata are backfilled onto
    // existing sites that lack coordinates." Pre-seed a site with NULL coords, then discover a
    // stream carrying coordinates for the same site, and apply.
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let site_id = uuid::Uuid::new_v4().to_string();
    crate::common::exec(
        &db,
        &format!("INSERT INTO sites (id, name) VALUES ('{site_id}', 'coordsite')"),
    )
    .await;

    let stream = uuid::Uuid::new_v4().to_string();
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &stream,
        "metalp",
        "k1",
        "METALP",
        "CoordSite",
        "Conductivity",
        "uS/cm",
        Some((45.9, 7.1, 1800.0)),
        0,
    )
    .await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({"source_system": "metalp"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create ({status}): {plan}");
    let plan_id = plan["id"].as_str().unwrap().to_string();
    run_plan_action(&app, &token, &plan_id, "apply").await;

    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM sites WHERE id = '{site_id}' AND latitude = 45.9 AND altitude_m = 1800.0"
        )).await,
        1, "coordinates backfilled onto the pre-existing coordinate-less site"
    );
}

/// A site is created once, so a value the source recorded wrong is corrected before the apply, not
/// on the site afterwards. The edit is per site: every feed at the station carries it, so which
/// entry the apply reads first cannot decide where the station is (M132).
#[tokio::test]
#[serial]
async fn a_site_created_by_the_plan_takes_the_elevation_the_review_corrected() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    // Two feeds at one station, both carrying the source's elevation of 1 m.
    let first = uuid::Uuid::new_v4().to_string();
    let second = uuid::Uuid::new_v4().to_string();
    for (stream, parameter) in [(&first, "Conductivity"), (&second, "Temperature")] {
        crate::common::seed_unpaired_stream_with_hierarchy(
            &db,
            stream,
            "metalp",
            &format!("wrongsite:{parameter}"),
            "METALP",
            "WrongElevation",
            parameter,
            "uS/cm",
            Some((46.25, 7.75, 1.0)),
            0,
        )
        .await;
    }
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({"source_system": "metalp"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create ({status}): {plan}");
    let plan_id = plan["id"].as_str().unwrap().to_string();

    let (status, body) = crate::common::patch_plan_with_token(
        &app,
        &plan_id,
        &serde_json::json!({
            "updates": [{ "stream_id": first, "site_altitude_m": 2100.0 }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "correct the elevation ({status}): {body}");

    let edited: serde_json::Value = serde_json::from_str(&body).expect("the plan comes back");
    for entry in edited["entries"].as_array().expect("entries") {
        assert_eq!(
            entry["site"]["altitude_m"],
            serde_json::json!(2100.0),
            "every feed at the station carries the correction: {entry}"
        );
    }

    run_plan_action(&app, &token, &plan_id, "apply").await;
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM sites WHERE LOWER(name) = 'wrongelevation' \
             AND altitude_m = 2100.0 AND latitude = 46.25"
        )
        .await,
        1,
        "the site is created at the corrected elevation, keeping the coordinates nobody edited"
    );
}

/// A site the plan resolved to an existing row is that site's own to edit: the apply only ever
/// backfills a coordinate it is missing, so a plan may not rewrite one it has.
#[tokio::test]
#[serial]
async fn an_existing_site_is_not_moved_by_a_plan_edit() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let site_id = uuid::Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO projects (id, name, data_source) \
             VALUES ('{}', 'METALP', 'test')",
            crate::common::PROJECT_ID
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sites (id, project_id, name, latitude, longitude, altitude_m) \
             VALUES ('{site_id}', '{}', 'Known Station', 46.1, 7.1, 500.0)",
            crate::common::PROJECT_ID
        ),
    )
    .await;

    let stream = uuid::Uuid::new_v4().to_string();
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &stream,
        "metalp",
        "known:Conductivity",
        "METALP",
        "Known Station",
        "Conductivity",
        "uS/cm",
        None,
        0,
    )
    .await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({"source_system": "metalp"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create ({status}): {plan}");
    let plan_id = plan["id"].as_str().unwrap().to_string();

    let (status, body) = crate::common::patch_plan_with_token(
        &app,
        &plan_id,
        &serde_json::json!({
            "updates": [{ "stream_id": stream, "site_altitude_m": 9999.0 }],
        }),
        &token,
    )
    .await;
    assert_eq!(
        status, 200,
        "the edit is accepted and ignored ({status}): {body}"
    );

    run_plan_action(&app, &token, &plan_id, "apply").await;
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM sites WHERE name = 'Known Station' AND altitude_m = 500.0"
        )
        .await,
        1,
        "the existing site keeps its own elevation"
    );
}
