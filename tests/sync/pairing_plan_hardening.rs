//! Pairing-plan entity resolution and apply-time guards: parameter matching by code/name/alias,
//! PATCH reclassification, empty-name rejection, created site_parameter naming/units, and the
//! already-paired stream skip.

use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;
use uuid::Uuid;

use crate::pairing_plan_apply::{job_id_of, wait_terminal};

async fn scalar_i64(db: &sea_orm::DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_owned(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "v")
    .unwrap()
}

async fn scalar_opt_string(db: &sea_orm::DatabaseConnection, sql: &str) -> Option<String> {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_owned(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<Option<String>>("", "v")
    .unwrap()
}

async fn scalar_opt_uuid(db: &sea_orm::DatabaseConnection, sql: &str) -> Option<Uuid> {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_owned(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<Option<Uuid>>("", "v")
    .unwrap()
}

async fn insert_plan(db: &sea_orm::DatabaseConnection, plan_id: Uuid, entries: &serde_json::Value) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO pairing_plans (id, source_system, status, summary, entries) \
             VALUES ('{plan_id}', 'hardening', 'draft', '{{}}'::jsonb, '{}'::jsonb)",
            entries.to_string().replace('\'', "''")
        ),
    )
    .await;
}

async fn apply_and_wait(
    app: &axum::Router,
    db: &sea_orm::DatabaseConnection,
    token: &str,
    plan_id: Uuid,
) {
    let (status, text) = crate::common::post_plan_action_with_token(
        app,
        &plan_id.to_string(),
        "apply",
        token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "apply should be 2xx, got {status}: {text}"
    );
    assert_eq!(wait_terminal(db, &job_id_of(&text)).await, "completed");
}

fn entry_for(plan: &serde_json::Value, stream_id: Uuid) -> serde_json::Value {
    plan["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["stream_id"] == serde_json::json!(stream_id))
        .cloned()
        .unwrap()
}

#[tokio::test]
#[serial]
async fn plan_matches_parameter_by_name_and_alias_case_insensitively() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let nitrate_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category, aliases) \
             VALUES ('{nitrate_id}', 'Nitrate', 'Nitrate', 'mg/L', 'measurement', ARRAY['NO3-N raw'])"
        ),
    )
    .await;

    let by_name = Uuid::new_v4();
    let by_alias = Uuid::new_v4();
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &by_name.to_string(),
        "hardening",
        "hard-name-1",
        "Test River Project",
        "Upstream Station",
        "water temperature",
        "°C",
        None,
        1,
    )
    .await;
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &by_alias.to_string(),
        "hardening",
        "hard-alias-1",
        "Test River Project",
        "Upstream Station",
        "no3-n RAW",
        "mg/L",
        None,
        1,
    )
    .await;

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({ "source_system": "hardening" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create plan failed: {plan}");

    let temp_id: Uuid = crate::common::GLOBAL_PARAM_TEMP_ID.parse().unwrap();
    let name_entry = entry_for(&plan, by_name);
    assert_eq!(name_entry["parameter"]["id"], serde_json::json!(temp_id));
    assert_eq!(name_entry["parameter"]["create"], serde_json::json!(false));
    let alias_entry = entry_for(&plan, by_alias);
    assert_eq!(
        alias_entry["parameter"]["id"],
        serde_json::json!(nitrate_id)
    );
    assert_eq!(alias_entry["parameter"]["create"], serde_json::json!(false));

    let params_before = scalar_i64(&db, "SELECT COUNT(*) AS v FROM parameters").await;
    let plan_id: Uuid = plan["id"].as_str().unwrap().parse().unwrap();
    apply_and_wait(&app, &db, &token, plan_id).await;

    let params_after = scalar_i64(&db, "SELECT COUNT(*) AS v FROM parameters").await;
    assert_eq!(
        params_after, params_before,
        "apply should not create any parameter"
    );
    let paired_param = scalar_opt_uuid(
        &db,
        &format!(
            "SELECT sp.parameter_id AS v FROM data_streams ds \
             JOIN site_parameters sp ON ds.site_parameter_id = sp.id WHERE ds.id = '{by_alias}'"
        ),
    )
    .await;
    assert_eq!(
        paired_param,
        Some(nitrate_id),
        "alias-named stream should pair to the existing parameter"
    );

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn patch_rename_reclassifies_entry_and_recomputes_warnings() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let stream_id = Uuid::new_v4();
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &stream_id.to_string(),
        "hardening",
        "hard-patch-1",
        "Test River Project",
        "Upstream Station",
        "Mystery Thing",
        "X",
        None,
        1,
    )
    .await;

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({ "source_system": "hardening" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create plan failed: {plan}");
    let plan_id = plan["id"].as_str().unwrap().to_string();
    let entry = entry_for(&plan, stream_id);
    assert_eq!(entry["parameter"]["create"], serde_json::json!(true));
    assert_eq!(entry["warnings"], serde_json::json!([]));

    // Mapping to an existing parameter with different units resolves the id and adds a warning
    let (status, text) = crate::common::patch_plan_with_token(
        &app,
        &plan_id.to_string(),
        &serde_json::json!({ "updates": [{ "stream_id": stream_id, "parameter_name": "Water Temperature" }] }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "patch failed: {text}");
    let updated: serde_json::Value = serde_json::from_str(&text).unwrap();
    let entry = entry_for(&updated, stream_id);
    let temp_id: Uuid = crate::common::GLOBAL_PARAM_TEMP_ID.parse().unwrap();
    assert_eq!(entry["parameter"]["id"], serde_json::json!(temp_id));
    assert_eq!(entry["parameter"]["create"], serde_json::json!(false));
    assert_eq!(entry["confidence"], serde_json::json!("exact"));
    let warnings = entry["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w["kind"] == "units_mismatch"),
        "unit mismatch warning expected, got {warnings:?}"
    );

    // Renaming away from the mismatch clears the warning and marks the parameter as new
    let (status, text) = crate::common::patch_plan_with_token(
        &app,
        &plan_id.to_string(),
        &serde_json::json!({ "updates": [{ "stream_id": stream_id, "parameter_name": "Brand New Param" }] }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "patch failed: {text}");
    let updated: serde_json::Value = serde_json::from_str(&text).unwrap();
    let entry = entry_for(&updated, stream_id);
    assert_eq!(entry["parameter"]["id"], serde_json::Value::Null);
    assert_eq!(entry["parameter"]["create"], serde_json::json!(true));
    assert_eq!(entry["warnings"], serde_json::json!([]));

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn pair_action_on_empty_parameter_name_is_rejected() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let patched = Uuid::new_v4();
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &patched.to_string(),
        "hardening",
        "hard-empty-1",
        "Test River Project",
        "Upstream Station",
        "",
        "",
        None,
        1,
    )
    .await;

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({ "source_system": "hardening" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create plan failed: {plan}");
    let plan_id = plan["id"].as_str().unwrap().to_string();

    let (status, text) = crate::common::patch_plan_with_token(
        &app,
        &plan_id.to_string(),
        &serde_json::json!({ "updates": [{ "stream_id": patched, "action": "pair" }] }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "patch failed: {text}");
    let updated: serde_json::Value = serde_json::from_str(&text).unwrap();
    let entry = entry_for(&updated, patched);
    assert_eq!(
        entry["action"],
        serde_json::json!("skip"),
        "pair with empty name must be forced to skip"
    );
    assert!(
        entry["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["kind"] == "empty_name"),
        "empty-name warning expected"
    );

    // A crafted entry that bypasses the PATCH guard is skipped at apply instead of
    // creating a blank parameter
    let forced = Uuid::new_v4();
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &forced.to_string(),
        "hardening",
        "hard-empty-2",
        "Test River Project",
        "Upstream Station",
        "",
        "",
        None,
        1,
    )
    .await;
    let forced_plan = Uuid::new_v4();
    insert_plan(&db, forced_plan, &serde_json::json!([{
        "stream_id": forced,
        "source_key": "hard-empty-2",
        "source_name": null,
        "action": "pair",
        "project": { "id": crate::common::PROJECT_ID, "name": "Test River Project", "create": false },
        "site": { "id": crate::common::SITE1_ID, "name": "Upstream Station", "create": false, "latitude": null, "longitude": null, "altitude_m": null },
        "parameter": { "id": null, "name": "", "create": true, "units": "", "group_key": null, "original_names": [] },
        "confidence": "none",
        "warnings": [],
        "original_parameter_name": null
    }]))
    .await;
    apply_and_wait(&app, &db, &token, forced_plan).await;

    let blank_params = scalar_i64(
        &db,
        "SELECT COUNT(*) AS v FROM parameters WHERE TRIM(code) = ''",
    )
    .await;
    assert_eq!(blank_params, 0, "no blank parameter should be created");
    assert!(
        scalar_opt_uuid(
            &db,
            &format!("SELECT site_parameter_id AS v FROM data_streams WHERE id = '{forced}'")
        )
        .await
        .is_none(),
        "stream with empty parameter name should stay unpaired"
    );
    let skipped = scalar_opt_string(
        &db,
        &format!("SELECT apply_result->>'streams_skipped' AS v FROM pairing_plans WHERE id = '{forced_plan}'"),
    )
    .await;
    assert_eq!(skipped.as_deref(), Some("1"));

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn created_site_parameter_carries_display_name_units_and_aliases() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let stream_id = Uuid::new_v4();
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &stream_id.to_string(),
        "hardening",
        "hard-create-1",
        "Test River Project",
        "Upstream Station",
        "NFlux",
        "mg/L",
        None,
        1,
    )
    .await;

    let plan_id = Uuid::new_v4();
    insert_plan(&db, plan_id, &serde_json::json!([{
        "stream_id": stream_id,
        "source_key": "hard-create-1",
        "source_name": null,
        "action": "pair",
        "project": { "id": crate::common::PROJECT_ID, "name": "Test River Project", "create": false },
        "site": { "id": crate::common::SITE1_ID, "name": "Upstream Station", "create": false, "latitude": null, "longitude": null, "altitude_m": null },
        "parameter": { "id": null, "name": "Nitrate Flux", "create": true, "units": "mg/L", "group_key": null, "original_names": ["NFlux", "Nitrate Flux"] },
        "confidence": "none",
        "warnings": [],
        "original_parameter_name": "NFlux"
    }]))
    .await;
    apply_and_wait(&app, &db, &token, plan_id).await;

    let sp_name = scalar_opt_string(
        &db,
        &format!(
            "SELECT sp.name AS v FROM data_streams ds \
             JOIN site_parameters sp ON ds.site_parameter_id = sp.id WHERE ds.id = '{stream_id}'"
        ),
    )
    .await;
    assert_eq!(
        sp_name.as_deref(),
        Some("Nitrate Flux"),
        "site_parameter should carry the display name"
    );
    let display_units = scalar_opt_string(
        &db,
        &format!(
            "SELECT sp.display_units AS v FROM data_streams ds \
             JOIN site_parameters sp ON ds.site_parameter_id = sp.id WHERE ds.id = '{stream_id}'"
        ),
    )
    .await;
    assert_eq!(display_units.as_deref(), Some("mg/L"));
    let units_name = scalar_opt_string(
        &db,
        &format!(
            "SELECT sp.units_name AS v FROM data_streams ds \
             JOIN site_parameters sp ON ds.site_parameter_id = sp.id WHERE ds.id = '{stream_id}'"
        ),
    )
    .await;
    assert_eq!(units_name.as_deref(), Some("mg/L"));

    let aliases = scalar_opt_string(
        &db,
        "SELECT array_to_string(aliases, ',') AS v FROM parameters WHERE code = 'Nitrate Flux'",
    )
    .await
    .unwrap();
    assert_eq!(
        aliases, "NFlux",
        "aliases should hold source names minus the chosen one"
    );

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn apply_skips_stream_paired_elsewhere_and_pairs_the_rest() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let taken = Uuid::new_v4();
    let free = Uuid::new_v4();
    for (id, key) in [(taken, "hard-taken-1"), (free, "hard-free-1")] {
        crate::common::seed_unpaired_stream_with_hierarchy(
            &db,
            &id.to_string(),
            "hardening",
            key,
            "Test River Project",
            "Upstream Station",
            "Water Temperature",
            "°C",
            None,
            1,
        )
        .await;
    }
    // Paired elsewhere between plan creation and apply
    crate::common::exec(
        &db,
        &format!(
            "UPDATE data_streams SET site_parameter_id = '{}' WHERE id = '{taken}'",
            crate::common::PARAM_S2_TEMP_ID
        ),
    )
    .await;

    let entry = |stream_id: Uuid, key: &str| {
        serde_json::json!({
            "stream_id": stream_id,
            "source_key": key,
            "source_name": null,
            "action": "pair",
            "project": { "id": crate::common::PROJECT_ID, "name": "Test River Project", "create": false },
            "site": { "id": crate::common::SITE1_ID, "name": "Upstream Station", "create": false, "latitude": null, "longitude": null, "altitude_m": null },
            "parameter": { "id": crate::common::GLOBAL_PARAM_TEMP_ID, "name": "Water Temperature", "create": false, "units": "°C", "group_key": null, "original_names": [] },
            "confidence": "exact",
            "warnings": [],
            "original_parameter_name": null
        })
    };
    let plan_id = Uuid::new_v4();
    insert_plan(
        &db,
        plan_id,
        &serde_json::json!([entry(taken, "hard-taken-1"), entry(free, "hard-free-1"),]),
    )
    .await;
    apply_and_wait(&app, &db, &token, plan_id).await;

    let taken_sp = scalar_opt_uuid(
        &db,
        &format!("SELECT site_parameter_id AS v FROM data_streams WHERE id = '{taken}'"),
    )
    .await;
    assert_eq!(
        taken_sp,
        Some(crate::common::PARAM_S2_TEMP_ID.parse().unwrap()),
        "already-paired stream must keep its pairing"
    );
    let free_sp = scalar_opt_uuid(
        &db,
        &format!("SELECT site_parameter_id AS v FROM data_streams WHERE id = '{free}'"),
    )
    .await;
    assert_eq!(
        free_sp,
        Some(crate::common::PARAM_S1_TEMP_ID.parse().unwrap()),
        "the other stream should still pair"
    );
    let counts = scalar_opt_string(
        &db,
        &format!(
            "SELECT (apply_result->>'streams_paired') || '/' || (apply_result->>'streams_skipped') AS v \
             FROM pairing_plans WHERE id = '{plan_id}'"
        ),
    )
    .await;
    assert_eq!(counts.as_deref(), Some("1/1"));

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn remap_to_existing_site_does_not_backfill_stream_coordinates() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let bare_site = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sites (id, project_id, name) VALUES ('{bare_site}', '{}', 'Bare Station')",
            crate::common::PROJECT_ID
        ),
    )
    .await;

    let stream_id = Uuid::new_v4();
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &stream_id.to_string(),
        "hardening",
        "hard-coords-1",
        "Test River Project",
        "Somewhere New",
        "Water Temperature",
        "°C",
        Some((46.5, 6.6, 372.0)),
        1,
    )
    .await;

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({ "source_system": "hardening" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create plan failed: {plan}");
    let plan_id: Uuid = plan["id"].as_str().unwrap().parse().unwrap();

    let (status, text) = crate::common::patch_plan_with_token(
        &app,
        &plan_id.to_string(),
        &serde_json::json!({ "updates": [{ "stream_id": stream_id, "site_name": "Bare Station" }] }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "patch failed: {text}");
    let updated: serde_json::Value = serde_json::from_str(&text).unwrap();
    let entry = entry_for(&updated, stream_id);
    assert_eq!(entry["site"]["id"], serde_json::json!(bare_site));
    assert_eq!(entry["site"]["latitude"], serde_json::Value::Null);

    apply_and_wait(&app, &db, &token, plan_id).await;

    let lat = scalar_opt_string(
        &db,
        &format!("SELECT latitude::text AS v FROM sites WHERE id = '{bare_site}'"),
    )
    .await;
    assert_eq!(
        lat, None,
        "remapped site must not inherit the stream's coordinates"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: a replicate family arrives under its incoming statistic column (`DOC_avg_ppb`),
/// with audit disagreements already recorded against the stream.
///
/// Expected behaviour: the plan suggests the family's own code, `DOC_ppb`, keeps the incoming name
/// as `original_parameter_name`, and quotes the divisor evidence: how many open disagreements there
/// are and how many match the population signature. Only the structural `avg` segment is dropped:
/// a catalog holding the shorter `DOC` is a different parameter, not this family's, so the units in
/// the column survive into the code every export writes.
#[tokio::test]
#[serial]
async fn a_replicate_family_keeps_its_own_code_and_quotes_divisor_evidence() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    // A shorter neighbour in the catalog, which the family must not be pulled onto.
    crate::common::exec(
        &db,
        "INSERT INTO parameters (id, code, name, default_units, category) \
         VALUES (gen_random_uuid(), 'DOC', 'Dissolved organic carbon', 'ppb', 'measurement')",
    )
    .await;

    let family = |param: &str| {
        format!(
            r#"{{"hierarchy": {{"project": "Test River Project", "site": "Upstream Station", "parameter": "{param}"}},
                "units": "ppb",
                "replicates": {{"source_columns": ["{param}_1", "{param}_2", "{param}_3"],
                                "portal_mean_column": "{param}", "portal_sd_column": "{param}_sd"}}}}"#
        )
    };
    let doc_stream = Uuid::new_v4();
    let other_stream = Uuid::new_v4();
    for (id, param) in [(doc_stream, "DOC_avg_ppb"), (other_stream, "Xfoo_avg")] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO data_streams (id, source_system, source_key, metadata, is_active) \
                 VALUES ('{id}', 'famsrc', 'STA:{param}:reps', '{}'::jsonb, true)",
                family(param)
            ),
        )
        .await;
    }

    // Two open disagreements on the DOC family: one whose incoming sd is exactly the population
    // form of the computed spread, one that matches neither divisor.
    for (hours_ago, expected_sd) in [(1, 0.816_496_580_927_726_f64), (2, 2.5)] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO replicate_audit_holds \
                     (stream_id, group_time, expected, computed, delta, status, kind) \
                 VALUES ('{doc_stream}', now() - interval '{hours_ago} hours', \
                         '{{\"mean\": 10.0, \"sd\": {expected_sd}, \"n\": 3}}', \
                         '{{\"mean\": 10.0, \"sd\": 1.0, \"n\": 3}}', '{{}}', 'deferred', \
                         'replicate_stats')"
            ),
        )
        .await;
    }

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({ "source_system": "famsrc" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create plan failed: {plan}");

    let doc = entry_for(&plan, doc_stream);
    assert_eq!(
        doc["parameter"]["name"],
        serde_json::json!("DOC_ppb"),
        "the family keeps its own units-bearing code: {doc}"
    );
    assert_eq!(
        doc["parameter"]["create"],
        serde_json::json!(true),
        "the catalog's shorter `DOC` is a different parameter, so this one is minted: {doc}"
    );
    assert_eq!(
        doc["original_parameter_name"],
        serde_json::json!("DOC_avg_ppb"),
        "the incoming column survives: {doc}"
    );
    assert_eq!(doc["sd_holds"], serde_json::json!(2), "{doc}");
    assert_eq!(doc["sd_population_holds"], serde_json::json!(1), "{doc}");
    assert!(
        doc["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["kind"] == "sd_estimator_undeclared"),
        "{doc}"
    );
    assert_eq!(
        doc["sd_estimator"],
        serde_json::Value::Null,
        "population-shaped evidence leaves the divisor for the review to answer: {doc}"
    );

    let other = entry_for(&plan, other_stream);
    assert_eq!(
        other["parameter"]["name"],
        serde_json::json!("Xfoo"),
        "no catalog match still drops the avg marker: {other}"
    );
    assert_eq!(other["sd_holds"], serde_json::json!(0), "{other}");
    assert_eq!(
        other["sd_estimator"],
        serde_json::Value::Null,
        "no disagreement is not a declaration; the family stays undeclared until the review \
         answers: {other}"
    );
    assert!(
        other["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["kind"] == "sd_estimator_undeclared"),
        "{other}"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: a replicate family lands on a slot that has already declared its divisor.
///
/// Expected behaviour: the entry adopts the slot's declaration and asks nothing, because the
/// answer somebody gave is the slot's and a plan must never rewrite it.
#[tokio::test]
#[serial]
async fn a_declared_slot_keeps_its_divisor_through_pairing() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    crate::common::exec(
        &db,
        &format!(
            "UPDATE site_parameters SET sd_estimator = 'population' WHERE id = '{}'",
            crate::common::PARAM_S1_DEPTH_ID
        ),
    )
    .await;

    let stream = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            r#"INSERT INTO data_streams (id, source_system, source_key, metadata, is_active)
               VALUES ('{stream}', 'declsrc', 'STA:Depth:reps',
                       '{{"hierarchy": {{"project": "Test River Project", "site": "Upstream Station", "parameter": "Depth"}},
                          "units": "mm",
                          "replicates": {{"source_columns": ["Depth_1", "Depth_2"],
                                          "portal_mean_column": "Depth", "portal_sd_column": "Depth_sd"}}}}'::jsonb,
                       true)"#
        ),
    )
    .await;

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({ "source_system": "declsrc" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create plan failed: {plan}");

    let entry = entry_for(&plan, stream);
    assert_eq!(
        entry["sd_estimator"],
        serde_json::json!("population"),
        "the slot's own declaration survives: {entry}"
    );
    assert!(
        entry["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["kind"] != "sd_estimator_undeclared"),
        "{entry}"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: a replicate family that ships its own sd column is paired before anything has been
/// ingested, so it has no audit holds at all.
///
/// Expected behaviour: the divisor is declared, never inferred. The plan leaves the family
/// undeclared and warns, and applying it leaves `site_parameters.sd_estimator` NULL, so the
/// undeclared-estimator gate and report still arm for the slot once population-shaped
/// disagreements arrive.
#[tokio::test]
#[serial]
async fn an_undisputed_family_stays_undeclared_through_apply() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let stream = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            r#"INSERT INTO data_streams (id, source_system, source_key, metadata, is_active)
               VALUES ('{stream}', 'undecl', 'STA:Depth:reps',
                       '{{"hierarchy": {{"project": "Test River Project", "site": "Upstream Station", "parameter": "Depth"}},
                          "units": "mm",
                          "replicates": {{"source_columns": ["Depth_1", "Depth_2"],
                                          "portal_mean_column": "Depth", "portal_sd_column": "Depth_sd"}}}}'::jsonb,
                       true)"#
        ),
    )
    .await;

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({ "source_system": "undecl" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create plan failed: {plan}");
    let entry = entry_for(&plan, stream);
    assert_eq!(entry["sd_holds"], serde_json::json!(0), "{entry}");
    assert_eq!(
        entry["sd_estimator"],
        serde_json::Value::Null,
        "no evidence is not a declaration: {entry}"
    );
    assert!(
        entry["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["kind"] == "sd_estimator_undeclared"),
        "the review is asked, with nothing presumed: {entry}"
    );

    let plan_id = Uuid::parse_str(plan["id"].as_str().unwrap()).unwrap();
    apply_and_wait(&app, &db, &token, plan_id).await;

    let declared = scalar_opt_string(
        &db,
        &format!(
            "SELECT sd_estimator AS v FROM site_parameters WHERE id = '{}'",
            crate::common::PARAM_S1_DEPTH_ID
        ),
    )
    .await;
    assert_eq!(
        declared, None,
        "apply writes no divisor the review did not choose"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// The draft is the operator's way back into a review, so the list must be filterable by source
/// and status and must not carry every draft's `entries` document with it.
#[tokio::test]
#[serial]
async fn plan_listing_filters_by_source_and_status_without_entries() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let entries = serde_json::json!([]);
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let other_source = Uuid::new_v4();
    insert_plan(&db, first, &entries).await;
    insert_plan(&db, second, &entries).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO pairing_plans (id, source_system, status, summary, entries) \
             VALUES ('{other_source}', 'elsewhere', 'draft', '{{}}'::jsonb, '[]'::jsonb)"
        ),
    )
    .await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        "/api/sync/pairing-plans?source_system=hardening&status=draft",
        &token,
    )
    .await;
    assert_eq!(status, 200, "listing failed: {body}");
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 2, "only the source's drafts: {body}");
    for row in rows {
        assert!(
            row.get("entries").is_none(),
            "the listing is a projection, not every draft's document: {row}"
        );
        assert!(row.get("summary").is_some(), "the summary is what it shows: {row}");
    }

    let (status, body) =
        crate::common::get_json_with_token(&app, "/api/sync/pairing-plans", &token).await;
    assert_eq!(status, 200, "unfiltered listing failed: {body}");
    assert_eq!(
        body.as_array().unwrap().len(),
        3,
        "no filter lists every plan: {body}"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// Start over supersedes the draft it replaces, and a superseded draft can no longer be applied.
#[tokio::test]
#[serial]
async fn superseded_plan_is_refused_by_apply() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let plan_id = Uuid::new_v4();
    insert_plan(&db, plan_id, &serde_json::json!([])).await;

    let (status, text) = crate::common::post_plan_action_with_token(
        &app,
        &plan_id.to_string(),
        "supersede",
        &token,
    )
    .await;
    assert_eq!(status, 200, "supersede failed: {text}");
    assert_eq!(
        scalar_opt_string(
            &db,
            &format!("SELECT status AS v FROM pairing_plans WHERE id = '{plan_id}'")
        )
        .await,
        Some("superseded".to_string())
    );

    let (status, text) = crate::common::post_plan_action_with_token(
        &app,
        &plan_id.to_string(),
        "apply",
        &token,
    )
    .await;
    assert!(
        !(200..300).contains(&status),
        "a superseded draft is not appliable: {text}"
    );

    let (status, text) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &serde_json::json!({ "expected_version": 0, "updates": [] }),
        &token,
    )
    .await;
    assert!(!(200..300).contains(&status), "nor editable: {text}");

    crate::common::cleanup_test_db(&db).await;
}

/// Two reviewers on one draft: the second write of a pair carrying the same version is refused,
/// so neither carries the other's decisions away without being told.
#[tokio::test]
#[serial]
async fn concurrent_plan_edits_conflict_on_the_version_they_read() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let first_stream = Uuid::new_v4();
    let second_stream = Uuid::new_v4();
    for (stream, key, parameter) in [
        (first_stream, "conc-1", "Depth"),
        (second_stream, "conc-2", "CDOM"),
    ] {
        crate::common::seed_unpaired_stream_with_hierarchy(
            &db,
            &stream.to_string(),
            "hardening",
            key,
            "Test River Project",
            "Upstream Station",
            parameter,
            "mm",
            None,
            1,
        )
        .await;
    }

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({ "source_system": "hardening" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create plan failed: {plan}");
    let plan_id = Uuid::parse_str(plan["id"].as_str().unwrap()).unwrap();
    let version = plan["version"].as_i64().expect("a plan carries its version");

    let (status, first) = crate::common::patch_json_parse_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &serde_json::json!({
            "expected_version": version,
            "updates": [{ "stream_id": first_stream, "action": "skip" }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "the first write lands: {first}");
    assert_eq!(
        first["version"].as_i64(),
        Some(version + 1),
        "an edit bumps the version: {first}"
    );

    let (status, text) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &serde_json::json!({
            "expected_version": version,
            "updates": [{ "stream_id": second_stream, "action": "skip" }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 409, "the second write of the pair is refused: {text}");

    let (status, plan) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{plan}");
    assert_eq!(
        entry_for(&plan, first_stream)["action"],
        serde_json::json!("skip"),
        "the first writer's decision stands"
    );
    assert_eq!(
        entry_for(&plan, second_stream)["action"],
        serde_json::json!("pair"),
        "and the refused write changed nothing"
    );

    // Applying against a version someone else has moved past is the same conflict.
    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}/apply"),
        &serde_json::json!({ "expected_version": version }),
        &token,
    )
    .await;
    assert_eq!(status, 409, "a stale apply is refused: {text}");

    crate::common::cleanup_test_db(&db).await;
}

/// A plan that is no longer a draft is a conflict, which is what the route has always documented.
#[tokio::test]
#[serial]
async fn non_draft_plan_refusals_are_conflicts() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let plan_id = Uuid::new_v4();
    insert_plan(&db, plan_id, &serde_json::json!([])).await;
    crate::common::exec(
        &db,
        &format!("UPDATE pairing_plans SET status = 'applied' WHERE id = '{plan_id}'"),
    )
    .await;

    let (status, text) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &serde_json::json!({ "expected_version": 0, "updates": [] }),
        &token,
    )
    .await;
    assert_eq!(status, 409, "an applied plan is not editable: {text}");

    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}/apply"),
        &serde_json::json!({ "expected_version": 0 }),
        &token,
    )
    .await;
    assert_eq!(status, 409, "nor appliable a second time: {text}");

    crate::common::cleanup_test_db(&db).await;
}

/// A plan-wide decision is one predicate on the wire: skip everything the catalog did not
/// recognise, without a line per entry, and pair back exactly that set with the opposite action.
#[tokio::test]
#[serial]
async fn a_bulk_decision_moves_every_entry_its_predicate_selects() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    // Two streams the catalog knows, one it does not.
    let known_a = Uuid::new_v4();
    let known_b = Uuid::new_v4();
    let unknown = Uuid::new_v4();
    for (stream, key, site, parameter) in [
        (known_a, "bulk-1", "Upstream Station", "Depth"),
        (known_b, "bulk-2", "Upstream Station", "Water Temperature"),
        (unknown, "bulk-3", "Nowhere Station", "Unheard Of"),
    ] {
        crate::common::seed_unpaired_stream_with_hierarchy(
            &db,
            &stream.to_string(),
            "hardening",
            key,
            "Test River Project",
            site,
            parameter,
            "mm",
            None,
            1,
        )
        .await;
    }

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({ "source_system": "hardening" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create plan failed: {plan}");
    let plan_id = Uuid::parse_str(plan["id"].as_str().unwrap()).unwrap();
    assert_eq!(
        entry_for(&plan, unknown)["confidence"],
        serde_json::json!("none"),
        "the unknown station is what the predicate selects"
    );

    let (status, body) = crate::common::patch_plan_with_token(
        &app,
        &plan_id.to_string(),
        &serde_json::json!({ "bulk": { "where": { "confidence": "none" }, "action": "skip" } }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "the bulk skip lands: {body}");
    let plan: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(entry_for(&plan, unknown)["action"], serde_json::json!("skip"));
    for stream in [known_a, known_b] {
        assert_eq!(
            entry_for(&plan, stream)["action"],
            serde_json::json!("pair"),
            "an entry the predicate does not select is untouched"
        );
    }

    let (status, body) = crate::common::patch_plan_with_token(
        &app,
        &plan_id.to_string(),
        &serde_json::json!({ "bulk": { "where": { "confidence": "none" }, "action": "pair" } }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "and the opposite action reverses it: {body}");
    let plan: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(entry_for(&plan, unknown)["action"], serde_json::json!("pair"));

    let (status, body) = crate::common::patch_plan_with_token(
        &app,
        &plan_id.to_string(),
        &serde_json::json!({ "bulk": { "where": {}, "action": "abandon" } }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "an action that is neither pair nor skip: {body}");

    crate::common::cleanup_test_db(&db).await;
}
