use river_db::routes::private::admin::merge_services::{MergeParametersRequest, merge_parameters};
use sea_orm::DatabaseConnection;
use serial_test::serial;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    use sea_orm::{ConnectionTrait, Statement};
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .ok()
    .flatten()
    .and_then(|r| r.try_get::<i64>("", "c").ok())
    .unwrap_or(0)
}

async fn aliases(db: &DatabaseConnection, param_id: &str) -> Vec<String> {
    use sea_orm::{ConnectionTrait, Statement};
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT aliases FROM parameters WHERE id = $1",
            [uuid::Uuid::parse_str(param_id).unwrap().into()],
        ))
        .await
        .ok()
        .flatten();
    row.and_then(|r| r.try_get::<Vec<String>>("", "aliases").ok())
        .unwrap_or_default()
}

fn merge_req(source: &str, target: &str) -> MergeParametersRequest {
    MergeParametersRequest {
        source_parameter_id: source.parse().unwrap(),
        target_parameter_id: target.parse().unwrap(),
    }
}

// Scenario: source parameter has a site_parameter at site1, target does not.
// Expected behaviour: site_parameter reassigned to target, readings moved, source deleted.
#[tokio::test]
#[serial]
async fn test_simple_reassign() {
    let (db, _, _) = setup().await;

    let total_before = count(&db, "SELECT count(*) AS c FROM readings").await;

    // TEMP is at both sites (via seed), DO is at both sites.
    // Remove target (TEMP) from site1 so only source (DO) is there for that param.
    crate::common::exec(
        &db,
        &format!(
            "UPDATE data_streams SET site_parameter_id = NULL WHERE site_parameter_id = '{}'",
            crate::common::PARAM_S1_TEMP_ID
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "DELETE FROM site_parameters WHERE id = '{}'",
            crate::common::PARAM_S1_TEMP_ID
        ),
    )
    .await;

    let before = count(
        &db,
        &format!(
            "SELECT count(*) AS c FROM readings WHERE parameter_id = '{}'",
            crate::common::GLOBAL_PARAM_DO_ID
        ),
    )
    .await;
    assert!(before > 0, "source should have readings");

    let result = merge_parameters(
        &db,
        &merge_req(
            crate::common::GLOBAL_PARAM_DO_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await
    .expect("merge should succeed");

    assert!(
        result.sites_reassigned > 0,
        "should reassign at least one site"
    );
    assert!(result.source_deleted);

    let source_exists = count(
        &db,
        &format!(
            "SELECT count(*) AS c FROM parameters WHERE id = '{}'",
            crate::common::GLOBAL_PARAM_DO_ID
        ),
    )
    .await;
    assert_eq!(source_exists, 0, "source parameter should be deleted");

    let moved = count(
        &db,
        &format!(
            "SELECT count(*) AS c FROM readings WHERE parameter_id = '{}'",
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    assert!(moved > 0, "readings should be on target now");

    let total_after = count(&db, "SELECT count(*) AS c FROM readings").await;
    assert_eq!(total_before, total_after, "zero readings lost");
}

// Scenario: both source and target have site_parameters at the same site.
// Expected behaviour: readings merged under target, source site_parameter deleted, zero data loss.
#[tokio::test]
#[serial]
async fn test_conflict_merge() {
    let (db, _, _) = setup().await;

    let total_readings_before = count(&db, "SELECT count(*) AS c FROM readings").await;

    let source_readings = count(
        &db,
        &format!(
            "SELECT count(*) AS c FROM readings WHERE parameter_id = '{}'",
            crate::common::GLOBAL_PARAM_DO_ID
        ),
    )
    .await;
    let target_readings = count(
        &db,
        &format!(
            "SELECT count(*) AS c FROM readings WHERE parameter_id = '{}'",
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    assert!(source_readings > 0 && target_readings > 0);

    let result = merge_parameters(
        &db,
        &merge_req(
            crate::common::GLOBAL_PARAM_DO_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await
    .expect("merge should succeed");

    assert!(result.sites_merged > 0, "should merge at conflicting sites");
    assert!(result.source_deleted);

    let source_sp = count(
        &db,
        &format!(
            "SELECT count(*) AS c FROM site_parameters WHERE parameter_id = '{}'",
            crate::common::GLOBAL_PARAM_DO_ID
        ),
    )
    .await;
    assert_eq!(source_sp, 0, "source site_parameters should be gone");

    let on_target = count(
        &db,
        &format!(
            "SELECT count(*) AS c FROM readings WHERE parameter_id = '{}'",
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    assert!(
        on_target >= source_readings + target_readings,
        "all readings should be under target: {on_target} >= {source_readings} + {target_readings}"
    );

    let total_readings_after = count(&db, "SELECT count(*) AS c FROM readings").await;
    assert_eq!(
        total_readings_before, total_readings_after,
        "zero readings lost: before={total_readings_before}, after={total_readings_after}"
    );
}

// Scenario: source (DEPTH) exists only at site1, target (TURB) exists at both sites.
// Expected behaviour: source's site_parameter at site1 merges into target's site1 entry,
// target's site2 entry is untouched, source parameter deleted.
#[tokio::test]
#[serial]
async fn test_cross_site() {
    let (db, _, _) = setup().await;

    // DEPTH is only at site1 (PARAM_S1_DEPTH_ID), TURB is at both sites.
    let result = merge_parameters(
        &db,
        &merge_req(
            crate::common::GLOBAL_PARAM_DEPTH_ID,
            crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    )
    .await
    .expect("merge should succeed");

    assert!(result.source_deleted);
    assert!(result.sites_merged + result.sites_reassigned > 0);

    let source_exists = count(
        &db,
        &format!(
            "SELECT count(*) AS c FROM parameters WHERE id = '{}'",
            crate::common::GLOBAL_PARAM_DEPTH_ID
        ),
    )
    .await;
    assert_eq!(source_exists, 0, "source parameter should be deleted");

    let target_sp_count = count(
        &db,
        &format!(
            "SELECT count(*) AS c FROM site_parameters WHERE parameter_id = '{}'",
            crate::common::GLOBAL_PARAM_TURB_ID
        ),
    )
    .await;
    assert!(
        target_sp_count >= 2,
        "target should still have site_params at both sites"
    );
}

// Scenario: a derived_parameter_source points to the source parameter.
// Expected behaviour: the source row's parameter_id is reassigned to target.
#[tokio::test]
#[serial]
async fn test_derived_sources_reassigned() {
    let (db, _, _) = setup().await;

    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO derived_parameter_definitions (id, code, name, units, formula)
             VALUES (gen_random_uuid(), 'test_derived', 'Test', 'mg/L', 'dissolved_oxygen * 0.032')
             ON CONFLICT DO NOTHING"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO derived_parameter_sources (derived_definition_id, parameter_id, variable_name)
             SELECT id, '{}', 'dissolved_oxygen'
             FROM derived_parameter_definitions WHERE code = 'test_derived'",
            crate::common::GLOBAL_PARAM_DO_ID
        ),
    )
    .await;

    merge_parameters(
        &db,
        &merge_req(
            crate::common::GLOBAL_PARAM_DO_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await
    .expect("merge should succeed");

    let reassigned = count(
        &db,
        &format!(
            "SELECT count(*) AS c FROM derived_parameter_sources WHERE parameter_id = '{}'",
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    assert!(reassigned > 0, "derived source should point to target");
}

// Scenario: source has aliases and a name; target has different aliases.
// Expected behaviour: target ends up with the union of both alias sets plus source's name.
#[tokio::test]
#[serial]
async fn test_aliases_absorbed() {
    let (db, _, _) = setup().await;

    crate::common::exec(
        &db,
        &format!(
            "UPDATE parameters SET aliases = ARRAY['MConduSCm', 'cond_raw'] WHERE id = '{}'",
            crate::common::GLOBAL_PARAM_DO_ID
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE parameters SET aliases = ARRAY['conductivity_us'] WHERE id = '{}'",
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;

    let source_code: String = {
        use sea_orm::{ConnectionTrait, Statement};
        db.query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT code FROM parameters WHERE id = $1",
            [uuid::Uuid::parse_str(crate::common::GLOBAL_PARAM_DO_ID)
                .unwrap()
                .into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "code")
        .unwrap()
    };

    merge_parameters(
        &db,
        &merge_req(
            crate::common::GLOBAL_PARAM_DO_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await
    .expect("merge should succeed");

    let target_aliases = aliases(&db, crate::common::GLOBAL_PARAM_TEMP_ID).await;
    assert!(
        target_aliases.contains(&"MConduSCm".to_string()),
        "should contain source alias: {target_aliases:?}"
    );
    assert!(
        target_aliases.contains(&"conductivity_us".to_string()),
        "should keep target alias: {target_aliases:?}"
    );
    assert!(
        target_aliases.contains(&source_code),
        "should contain source code '{source_code}': {target_aliases:?}"
    );
}

// Scenario: source_parameter_id == target_parameter_id.
// Expected behaviour: 400 error.
#[tokio::test]
#[serial]
async fn test_same_id_rejection() {
    let (db, _, _) = setup().await;
    let result = merge_parameters(
        &db,
        &merge_req(
            crate::common::GLOBAL_PARAM_TEMP_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await;
    assert!(result.is_err(), "should reject same-id merge");
}

// Scenario: source does not exist.
// Expected behaviour: 404 error.
#[tokio::test]
#[serial]
async fn test_not_found_rejection() {
    let (db, _, _) = setup().await;
    let result = merge_parameters(
        &db,
        &merge_req(
            "00000000-0000-4000-b000-999999999999",
            crate::common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await;
    assert!(result.is_err(), "should reject nonexistent source");
}

// Scenario: call merge via HTTP endpoint, which runs as a tracked merge_parameters job.
// Expected behaviour: 200 with a job_id, job completes, source actually deleted.
#[tokio::test]
#[serial]
async fn test_http_round_trip() {
    let (db, app, token) = setup().await;

    let body = serde_json::json!({
        "source_parameter_id": crate::common::GLOBAL_PARAM_DO_ID,
        "target_parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
    });

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/merge_parameters",
        &body,
        &token,
    )
    .await;

    assert_eq!(status, 200, "merge via HTTP: {resp}");
    let job_id = resp["job_id"]
        .as_str()
        .expect("response carries job_id")
        .to_string();
    assert_eq!(
        crate::merge_site_parameters_job::wait_terminal(&db, &job_id).await,
        "completed"
    );

    let (get_status, _) = crate::common::get_with_token(
        &app,
        &format!("/api/parameters/{}", crate::common::GLOBAL_PARAM_DO_ID),
        &token,
    )
    .await;
    assert_eq!(get_status, 404, "source parameter should be gone");
}
