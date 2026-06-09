//! Pairing-plan workflow (USER_STORIES "Stream Pairing Workflow") over the HTTP surface: grouped
//! discovery, then the draft → inspect → update → apply → revert lifecycle, plus the apply-time
//! entity-resolution rules (reuse existing site by case-insensitive name, reuse parameter via alias,
//! coordinate backfill). A single full-permission API token drives every route.
//!
//! Apply spawns a post-commit reprocess + aggregate refresh; assertions here only touch the
//! synchronously-committed facts (pairing, backfilled site_id/parameter_id, plan status,
//! ApplyResult). Revert refreshes aggregates synchronously, so its assertions are immediately
//! consistent.
//!
//! Run: cargo test --test e2e -- --test-threads=1


use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    let row = db
        .query_one(Statement::from_string(DatabaseBackend::Postgres, sql.to_string()))
        .await
        .expect("query")
        .expect("row");
    row.try_get::<i64>("", "c").expect("c")
}

fn find_entry<'a>(plan: &'a serde_json::Value, stream_id: &str) -> &'a serde_json::Value {
    plan["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("entries array: {plan}"))
        .iter()
        .find(|e| e["stream_id"] == serde_json::json!(stream_id))
        .unwrap_or_else(|| panic!("entry for stream {stream_id} missing: {plan}"))
}

#[tokio::test]
#[serial]
async fn grouped_discovery_groups_unpaired_streams_by_site() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db, &uuid::Uuid::new_v4().to_string(), "nomis", "k1",
        "NOMIS", "GL1_DN", "Conductivity", "uS/cm", Some((46.5, 7.9, 2400.0)), 0,
    ).await;
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db, &uuid::Uuid::new_v4().to_string(), "nomis", "k2",
        "NOMIS", "GL1_DN", "Temperature", "degC", None, 0,
    ).await;
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db, &uuid::Uuid::new_v4().to_string(), "nomis", "k3",
        "NOMIS", "GL2_UP", "Conductivity", "uS/cm", None, 0,
    ).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/grouped-discovery",
        &serde_json::json!({"source_system": "nomis"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grouped-discovery ({status}): {resp}");
    assert_eq!(resp["total_streams"], 3);

    let sites = resp["sites"].as_array().expect("sites array");
    assert_eq!(sites.len(), 2, "two distinct sites: {resp}");
    let gl1 = sites.iter().find(|s| s["name"] == "GL1_DN").expect("GL1_DN");
    assert_eq!(gl1["stream_count"], 2, "GL1_DN has two streams");
    assert!(gl1["existing_id"].is_null(), "GL1_DN does not pre-exist");
    let gl2 = sites.iter().find(|s| s["name"] == "GL2_UP").expect("GL2_UP");
    assert_eq!(gl2["stream_count"], 1);

    let params = resp["parameters"].as_array().expect("parameters array");
    assert!(params.iter().any(|p| p["name"] == "Conductivity"), "Conductivity grouped: {resp}");
    assert!(params.iter().any(|p| p["name"] == "Temperature"), "Temperature grouped: {resp}");
}

#[tokio::test]
#[serial]
async fn create_inspect_update_apply_revert_full_lifecycle() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let cond1 = uuid::Uuid::new_v4().to_string();
    let temp1 = uuid::Uuid::new_v4().to_string();
    let cond2 = uuid::Uuid::new_v4().to_string();
    let temp2 = uuid::Uuid::new_v4().to_string();
    crate::common::seed_unpaired_stream_with_hierarchy(&db, &cond1, "nomis", "c1", "NOMIS", "GL1_DN", "Conductivity", "uS/cm", None, 3).await;
    crate::common::seed_unpaired_stream_with_hierarchy(&db, &temp1, "nomis", "t1", "NOMIS", "GL1_DN", "Temperature", "degC", None, 3).await;
    crate::common::seed_unpaired_stream_with_hierarchy(&db, &cond2, "nomis", "c2", "NOMIS", "GL2_UP", "Conductivity", "uS/cm", None, 3).await;
    crate::common::seed_unpaired_stream_with_hierarchy(&db, &temp2, "nomis", "t2", "NOMIS", "GL2_UP", "Temperature", "degC", None, 3).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    // create
    let (status, plan) = crate::common::post_json_parse_with_token(
        &app, "/api/sync/pairing-plans", &serde_json::json!({"source_system": "nomis"}), &token,
    ).await;
    assert_eq!(status, 200, "create plan ({status}): {plan}");
    assert_eq!(plan["status"], "draft");
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    let s = &plan["summary"];
    assert_eq!(s["total_streams"], 4);
    assert_eq!(s["will_pair"], 4);
    assert_eq!(s["will_skip"], 0);
    assert_eq!(s["sites_to_create"], 2);
    assert_eq!(s["parameters_to_create"], 2, "Conductivity + Temperature: {plan}");

    // inspect
    let (status, plan) = crate::common::get_json_with_token(&app, &format!("/api/sync/pairing-plans/{plan_id}"), &token).await;
    assert_eq!(status, 200);
    assert_eq!(plan["entries"].as_array().unwrap().len(), 4);
    let e = find_entry(&plan, &cond1);
    assert_eq!(e["action"], "pair");
    assert_eq!(e["project"]["name"], "NOMIS");
    assert_eq!(e["site"]["create"], true);
    assert_eq!(e["parameter"]["create"], true);
    assert_eq!(e["confidence"], "none");

    // an existing parameter to map the Conductivity streams onto (seeded AFTER create so the plan
    // proposed creating it; the update points the entries at the real row by id)
    let cond_param = uuid::Uuid::new_v4().to_string();
    crate::common::exec(&db, &format!(
        "INSERT INTO parameters (id, code, name, default_units, category) \
         VALUES ('{cond_param}', 'conductivity', 'Conductivity', 'uS/cm', 'measurement')",
    )).await;

    // update: map both Conductivity entries to the existing param
    let (status, body) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &serde_json::json!({"updates": [
            {"stream_id": cond1, "parameter_id": cond_param},
            {"stream_id": cond2, "parameter_id": cond_param}
        ]}),
        &token,
    ).await;
    assert_eq!(status, 200, "update plan ({status}): {body}");
    let plan: serde_json::Value = serde_json::from_str(&body).unwrap();
    let e = find_entry(&plan, &cond1);
    assert_eq!(e["parameter"]["id"], serde_json::json!(cond_param), "mapped to existing param");
    assert_eq!(e["parameter"]["create"], false);

    // apply
    let (status, res) = crate::common::post_json_parse_with_token(
        &app, &format!("/api/sync/pairing-plans/{plan_id}/apply"), &serde_json::json!({}), &token,
    ).await;
    assert_eq!(status, 200, "apply ({status}): {res}");
    assert_eq!(res["streams_paired"], 4);
    assert_eq!(res["sites_created"], 2);
    assert_eq!(res["projects_created"], 1);
    assert_eq!(res["parameters_created"], 1, "only Temperature is new; Conductivity reused: {res}");
    assert_eq!(res["site_parameters_created"], 4);
    assert_eq!(res["readings_backfilled"], 12);

    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM data_streams WHERE pairing_plan_id = '{plan_id}' AND site_parameter_id IS NOT NULL"
        )).await,
        4, "all four streams paired under the plan"
    );
    assert_eq!(
        count(&db, "SELECT count(*) AS c FROM parameters WHERE LOWER(code) = 'conductivity'").await,
        1, "the reused parameter was not duplicated"
    );
    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM readings r JOIN data_streams ds ON r.stream_id = ds.id \
             WHERE ds.pairing_plan_id = '{plan_id}' AND r.site_id IS NOT NULL"
        )).await,
        12, "readings backfilled with site_id"
    );
    let (_, plan) = crate::common::get_json_with_token(&app, &format!("/api/sync/pairing-plans/{plan_id}"), &token).await;
    assert_eq!(plan["status"], "applied");

    // re-apply rejected
    let (status, _) = crate::common::post_json_with_token(
        &app, &format!("/api/sync/pairing-plans/{plan_id}/apply"), &serde_json::json!({}), &token,
    ).await;
    assert_eq!(status, 400, "cannot re-apply an applied plan");

    // revert
    let (status, body) = crate::common::post_json_parse_with_token(
        &app, &format!("/api/sync/pairing-plans/{plan_id}/revert"), &serde_json::json!({}), &token,
    ).await;
    assert_eq!(status, 200, "revert ({status}): {body}");
    assert_eq!(body["reverted"], 4);
    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM readings r JOIN data_streams ds ON r.stream_id = ds.id \
             WHERE ds.source_system = 'nomis' AND r.site_id IS NOT NULL"
        )).await,
        0, "revert re-NULLed the backfilled readings"
    );
    assert_eq!(
        count(&db, "SELECT count(*) AS c FROM data_streams WHERE source_system = 'nomis' AND site_parameter_id IS NULL").await,
        4, "all streams unpaired again"
    );
    assert!(
        count(&db, "SELECT count(*) AS c FROM sites WHERE LOWER(name) IN ('gl1_dn','gl2_up')").await >= 2,
        "created catalog sites are retained after revert"
    );
    let (_, plan) = crate::common::get_json_with_token(&app, &format!("/api/sync/pairing-plans/{plan_id}"), &token).await;
    assert_eq!(plan["status"], "reverted");

    // re-revert rejected
    let (status, _) = crate::common::post_json_with_token(
        &app, &format!("/api/sync/pairing-plans/{plan_id}/revert"), &serde_json::json!({}), &token,
    ).await;
    assert_eq!(status, 400, "cannot revert a non-applied plan");
}

#[tokio::test]
#[serial]
async fn apply_reuses_existing_site_by_case_insensitive_name() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let project_id = uuid::Uuid::new_v4().to_string();
    let site_id = uuid::Uuid::new_v4().to_string();
    crate::common::exec(&db, &format!("INSERT INTO projects (id, name) VALUES ('{project_id}', 'NOMIS')")).await;
    crate::common::exec(&db, &format!(
        "INSERT INTO sites (id, project_id, name, latitude, longitude) VALUES ('{site_id}', '{project_id}', 'gl1_dn', 46.0, 7.0)"
    )).await;

    let stream = uuid::Uuid::new_v4().to_string();
    crate::common::seed_unpaired_stream_with_hierarchy(&db, &stream, "nomis", "k1", "NOMIS", "GL1_DN", "Conductivity", "uS/cm", None, 0).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app, "/api/sync/pairing-plans", &serde_json::json!({"source_system": "nomis"}), &token,
    ).await;
    assert_eq!(status, 200, "create ({status}): {plan}");
    let plan_id = plan["id"].as_str().unwrap().to_string();

    let (status, res) = crate::common::post_json_parse_with_token(
        &app, &format!("/api/sync/pairing-plans/{plan_id}/apply"), &serde_json::json!({}), &token,
    ).await;
    assert_eq!(status, 200, "apply ({status}): {res}");
    assert_eq!(res["sites_created"], 0, "existing site reused by case-insensitive name");
    assert_eq!(res["projects_created"], 0, "existing project reused");

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
    crate::common::seed_unpaired_stream_with_hierarchy(&db, &stream, "nomis", "k1", "NOMIS", "GL1_DN", "Conductivity", "uS/cm", None, 0).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app, "/api/sync/pairing-plans", &serde_json::json!({"source_system": "nomis"}), &token,
    ).await;
    assert_eq!(status, 200, "create ({status}): {plan}");
    let plan_id = plan["id"].as_str().unwrap().to_string();

    let (status, res) = crate::common::post_json_parse_with_token(
        &app, &format!("/api/sync/pairing-plans/{plan_id}/apply"), &serde_json::json!({}), &token,
    ).await;
    assert_eq!(status, 200, "apply ({status}): {res}");
    assert_eq!(res["parameters_created"], 0, "parameter reused via alias match");
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
        &db, &stream, "nomis", "k1", "NOMIS", "NewSite", "Conductivity", "uS/cm", Some((46.25, 7.75, 2100.0)), 0,
    ).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app, "/api/sync/pairing-plans", &serde_json::json!({"source_system": "nomis"}), &token,
    ).await;
    assert_eq!(status, 200, "create ({status}): {plan}");
    let plan_id = plan["id"].as_str().unwrap().to_string();
    let (status, res) = crate::common::post_json_parse_with_token(
        &app, &format!("/api/sync/pairing-plans/{plan_id}/apply"), &serde_json::json!({}), &token,
    ).await;
    assert_eq!(status, 200, "apply ({status}): {res}");

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
    crate::common::exec(&db, &format!("INSERT INTO sites (id, name) VALUES ('{site_id}', 'coordsite')")).await;

    let stream = uuid::Uuid::new_v4().to_string();
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db, &stream, "nomis", "k1", "NOMIS", "CoordSite", "Conductivity", "uS/cm", Some((45.9, 7.1, 1800.0)), 0,
    ).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app, "/api/sync/pairing-plans", &serde_json::json!({"source_system": "nomis"}), &token,
    ).await;
    assert_eq!(status, 200, "create ({status}): {plan}");
    let plan_id = plan["id"].as_str().unwrap().to_string();
    let (status, res) = crate::common::post_json_parse_with_token(
        &app, &format!("/api/sync/pairing-plans/{plan_id}/apply"), &serde_json::json!({}), &token,
    ).await;
    assert_eq!(status, 200, "apply ({status}): {res}");

    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM sites WHERE id = '{site_id}' AND latitude = 45.9 AND altitude_m = 1800.0"
        )).await,
        1, "coordinates backfilled onto the pre-existing coordinate-less site"
    );
}
