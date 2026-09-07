//! End-to-end derived-parameter lifecycle: define a derived parameter, confirm the `sources` join
//! populates (the WS1c fk_column fix), assign it to a site, and verify the assignment backfills
//! historical derived readings and exposes them publicly.
//!
//! Run: cargo test --test derived_parameters -- --test-threads=1

use crate::common::e2e;
use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;

/// Create the DOmgL derived definition over the seeded Dissolved_O2 source; returns (def_id, output_param_id).
async fn create_derived(app: &axum::Router, token: &str) -> (String, String) {
    let (status, def) = crate::common::post_json_parse_with_token(
        app,
        "/api/derived_parameters",
        &serde_json::json!({
            "code": "DOmgL_e2e", "name": "DO mg/L (e2e)", "units": "mg/L",
            "formula": "Dissolved_O2 * 0.032",
        }),
        token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create derived ({status}): {def}"
    );
    let output = def["output_parameter_id"]
        .as_str()
        .expect("output_parameter_id")
        .to_string();
    (e2e::id_of(&def), output)
}

/// Supported today: a derived definition populates `sources` on GET-by-id and list, and can be
/// assigned to a site.
#[tokio::test]
#[serial]
async fn derived_definition_populates_sources_and_assigns() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let (def_id, output_param_id) = create_derived(&app, &token).await;

    // WS1c: the join populates `sources` on GET-by-id AND in the list endpoint.
    let (_s, got) = crate::common::get_json_with_token(
        &app,
        &format!("/api/derived_parameters/{def_id}"),
        &token,
    )
    .await;
    let sources = got["sources"]
        .as_array()
        .expect("sources array on GET-by-id");
    assert_eq!(sources.len(), 1, "exactly one source (Dissolved_O2): {got}");
    assert_eq!(sources[0]["variable_name"], "Dissolved_O2");

    let (_s, list) =
        crate::common::get_json_with_token(&app, "/api/derived_parameters?page_size=100", &token)
            .await;
    let items = list
        .as_array()
        .cloned()
        .unwrap_or_else(|| list["data"].as_array().cloned().unwrap());
    let in_list = items
        .iter()
        .find(|d| d["id"].as_str() == Some(def_id.as_str()))
        .expect("def in list");
    assert_eq!(
        in_list["sources"].as_array().map(|a| a.len()),
        Some(1),
        "sources populated in the list"
    );

    // Assigning the derived parameter to a site succeeds.
    let (status, sp) = crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID, "parameter_id": output_param_id, "name": "DOmgL_e2e",
            "sensor_type": "derived", "is_derived": true, "derived_definition_id": def_id, "display_units": "mg/L",
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "assign derived ({status}): {sp}"
    );
}

/// Assigning a derived parameter backfills historical derived readings (= source × factor) and
/// exposes them publicly once the project + site_parameter are made public.
#[tokio::test]
#[serial]
async fn derived_assignment_backfills_and_publishes() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let (def_id, output_param_id) = create_derived(&app, &token).await;

    let (status, sp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID, "parameter_id": output_param_id, "name": "DOmgL_e2e",
            "sensor_type": "derived", "is_derived": true, "derived_definition_id": def_id, "display_units": "mg/L",
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "assign derived ({status}): {sp}"
    );
    let sp_id = e2e::id_of(&sp);

    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "derived_assignment", 30).await,
        "derived_assignment backfill should run and complete"
    );

    // Backfilled derived readings equal source × 0.032 at matching timestamps.
    let uri = format!(
        "/api/sites/{}/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T01:00:00Z",
        crate::common::SITE1_ID
    );
    let (status, readings) = crate::common::get_json_with_token(&app, &uri, &token).await;
    assert_eq!(status, 200, "readings ({status}): {readings}");
    let src = e2e::values_for(&readings, crate::common::GLOBAL_PARAM_DO_ID);
    let derived = e2e::values_for(&readings, &output_param_id);
    assert!(!src.is_empty(), "expected seeded source readings");
    assert_eq!(src.len(), derived.len());
    for i in 0..src.len() {
        assert!(
            (derived[i] - src[i] * 0.032).abs() < 1e-6,
            "derived[{i}] should be source × 0.032"
        );
    }

    // Recompute + public exposure.
    let (status, _r) = crate::common::post_json_with_token(
        &app,
        &format!("/api/actions/derived_parameters/{def_id}/recompute"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "recompute ({status})");

    db.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "UPDATE projects SET is_public = true, public_code = 'e2e_derived' WHERE id = '{}'",
            crate::common::PROJECT_ID
        ),
    ))
    .await
    .unwrap();
    // A site is only included in the public config when it has a public_code (services.rs load_public_config).
    db.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "UPDATE sites SET public_code = 'e2e_derived_site1' WHERE id = '{}'",
            crate::common::SITE1_ID
        ),
    ))
    .await
    .unwrap();
    e2e::set_site_parameter_public(&db, &sp_id).await;

    let pub_uri = "/api/public/e2e_derived/sites/e2e_derived_site1/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T01:00:00Z";
    let (status, pub_readings) = crate::common::get_json(&app, &pub_uri).await;
    assert_eq!(status, 200, "public readings ({status}): {pub_readings}");
    assert!(
        !e2e::values_for(&pub_readings, "DOmgL_e2e").is_empty(),
        "derived exposed publicly"
    );
}

/// A `running` job of the given trigger at a site, leased far enough ahead that the worker's
/// reaper leaves it alone for the length of the test.
async fn insert_running_job(db: &sea_orm::DatabaseConnection, trigger_type: &str, site_id: &str) {
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO reprocessing_jobs \
             (id, trigger_type, status, category, params, detail, owner, lease_epoch, \
              lease_expires_at) \
         VALUES (gen_random_uuid(), $1, 'running', 'data', '{}'::jsonb, \
                 jsonb_build_object('scope', jsonb_build_object('site_id', $2::text)), \
                 'another-replica', 1, now() + interval '1 hour')",
        [trigger_type.into(), site_id.into()],
    ))
    .await
    .expect("insert running job");
}

#[tokio::test]
#[serial]
async fn an_unrelated_sites_import_does_not_suppress_the_assignment_backfill() {
    // Scenario: a CSV import is running at SITE2 while a derived parameter is assigned at SITE1.
    // Expected behaviour: the derived_assignment job is enqueued and completes; the other site's
    // job has nothing to do with the rows this one produces.
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    insert_running_job(&db, "csv_import", crate::common::SITE2_ID).await;

    let (def_id, output_param_id) = create_derived(&app, &token).await;
    let (status, sp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID, "parameter_id": output_param_id, "name": "DOmgL_e2e",
            "sensor_type": "derived", "is_derived": true, "derived_definition_id": def_id, "display_units": "mg/L",
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "assign derived ({status}): {sp}"
    );

    let enqueued = e2e::count(
        &db,
        &format!("SELECT COUNT(*) FROM reprocessing_jobs WHERE trigger_type = 'derived_assignment' AND trigger_id = '{def_id}'"),
    )
    .await;
    assert_eq!(
        enqueued, 1,
        "the backfill is enqueued regardless of the other site's import"
    );
    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "derived_assignment", 30).await,
        "and it completes"
    );
}

#[tokio::test]
#[serial]
async fn the_same_definitions_in_flight_backfill_is_not_duplicated() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let (def_id, output_param_id) = create_derived(&app, &token).await;
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO reprocessing_jobs \
             (id, trigger_type, trigger_id, status, category, params, owner, lease_epoch, \
              lease_expires_at) \
         VALUES (gen_random_uuid(), 'derived_assignment', $1::uuid, 'running', 'data', \
                 '{}'::jsonb, 'another-replica', 1, now() + interval '1 hour')",
        [def_id.clone().into()],
    ))
    .await
    .unwrap();
    let (status, sp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID, "parameter_id": output_param_id, "name": "DOmgL_e2e",
            "sensor_type": "derived", "is_derived": true, "derived_definition_id": def_id, "display_units": "mg/L",
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "assign derived ({status}): {sp}"
    );
    let rows = e2e::count(
        &db,
        &format!("SELECT COUNT(*) FROM reprocessing_jobs WHERE trigger_type = 'derived_assignment' AND trigger_id = '{def_id}'"),
    )
    .await;
    assert_eq!(
        rows, 1,
        "this definition's own in-flight backfill is not doubled"
    );
}

/// Scenario: two sites hold the same output parameter, and only one declares its slot computed.
/// Expected behaviour: the declaring site gets computed values and the other is left alone. The
/// declaration is per site, while group membership and the calculation that produces a parameter
/// are global, so this is what stops a global rule computing over a site that measures by hand.
#[tokio::test]
#[serial]
async fn a_site_that_does_not_declare_the_slot_computed_is_left_alone() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let (def_id, output_param_id) = create_derived(&app, &token).await;

    // The second site holds the same output parameter as an ordinary slot, entered by hand.
    let (status, other) = crate::common::post_json_parse_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE2_ID, "parameter_id": output_param_id,
            "name": "DOmgL_e2e", "display_units": "mg/L",
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "assign plain slot ({status}): {other}"
    );

    let (status, sp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID, "parameter_id": output_param_id, "name": "DOmgL_e2e",
            "sensor_type": "derived", "is_derived": true, "derived_definition_id": def_id,
            "display_units": "mg/L",
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "assign derived ({status}): {sp}"
    );

    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "derived_assignment", 30).await,
        "derived_assignment backfill should run and complete"
    );

    let computed_here = count_readings(&db, crate::common::SITE1_ID, &output_param_id).await;
    assert!(
        computed_here > 0,
        "the site that declares the slot computed gets values"
    );

    let computed_there = count_readings(&db, crate::common::SITE2_ID, &output_param_id).await;
    assert_eq!(
        computed_there, 0,
        "the site that did not declare it computes nothing, it measures this parameter by hand"
    );
}

async fn count_readings(db: &sea_orm::DatabaseConnection, site_id: &str, parameter_id: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT count(*) AS c FROM readings \
              WHERE site_id = '{site_id}' AND parameter_id = '{parameter_id}'"
        ),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "c")
    .unwrap()
}
