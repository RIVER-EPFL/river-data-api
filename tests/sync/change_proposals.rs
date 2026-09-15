//! Scenario: a value the source changed after river-data stored it is proposed, not written, and a
//! person decides it (Q84).
//!
//! Expected behaviour: the queue and the decision are confined to the caller's projects. A manager
//! granted one project neither lists nor decides another project's proposal, however the id reaches
//! them, and the ids they may not decide come back refused rather than silently applied. A
//! project-scoped API token never reaches these routes at all (`deny_scoped_token` on the manage
//! layer), so the caller who can be restricted here is a person with grants.

use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_TEMP_ID, PARAM_S1_TEMP_ID, PROJECT_ID};

const PROJECT_B: &str = "00000000-0000-4000-c000-000000000001";
const SITE_B: &str = "00000000-0000-4000-c000-000000000010";
const SP_B: &str = "00000000-0000-4000-c000-000000000101";
const AT: &str = "2025-06-01T08:00:00Z";

/// A paired stream and one proposed correction on it, without going through a sync cycle: the
/// question here is who may decide, not how the proposal was raised.
async fn proposal_on(
    db: &sea_orm::DatabaseConnection,
    site_parameter_id: &str,
    source_key: &str,
) -> Uuid {
    let stream = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, site_parameter_id, is_active) \
             VALUES ('{stream}', 'scopesrc', '{source_key}', '{site_parameter_id}', true)"
        ),
    )
    .await;
    let id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO reading_change_proposals \
                 (id, stream_id, time, replicate_index, proposed_raw_value, stored_raw_value) \
             VALUES ('{id}', '{stream}', '{AT}', 0, 11.5, 10.0)"
        ),
    )
    .await;
    id
}

async fn status_of(db: &sea_orm::DatabaseConnection, id: Uuid) -> String {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT status FROM reading_change_proposals WHERE id = '{id}'"),
    ))
    .await
    .unwrap()
    .expect("the proposal")
    .try_get::<String>("", "status")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn a_granted_manager_decides_their_own_projects_proposals_and_no_others() {
    if !crate::common::keycloak::keycloak_reachable().await {
        eprintln!("SKIP: keycloak unreachable (start the dev stack, or set TEST_KEYCLOAK_URL)");
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (app, state) =
        crate::common::keycloak::build_test_app_with_keycloak_and_state(db.clone()).await;

    for sql in [
        format!(
            "INSERT INTO projects (id, name, description) VALUES ('{PROJECT_B}', 'Elsewhere', 'scope check')"
        ),
        format!(
            "INSERT INTO sites (id, name, project_id) VALUES ('{SITE_B}', 'ScopeSiteB', '{PROJECT_B}')"
        ),
        format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type) \
             VALUES ('{SP_B}', '{SITE_B}', '{GLOBAL_PARAM_TEMP_ID}', 'TempB', 'sensor')"
        ),
    ] {
        crate::common::exec(&db, &sql).await;
    }

    let mine = proposal_on(&db, PARAM_S1_TEMP_ID, "scoped-a").await;
    let theirs = proposal_on(&db, SP_B, "scoped-b").await;

    // manager1 holds the manager capability globally and is granted one project.
    let sub = crate::common::keycloak::keycloak_user_id("manager1").await;
    crate::common::keycloak::grant_project(&db, &sub, PROJECT_ID).await;
    state.grants_cache.invalidate_all();
    let jwt = crate::common::keycloak::get_keycloak_jwt("manager1", "manager1").await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/reading_change_proposals?filter={}",
            crate::common::e2e::percent_encode(r#"{"status":"pending"}"#)
        ),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "list ({status}): {body}");
    let listed: Vec<String> = body
        .as_array()
        .expect("a list of proposals")
        .iter()
        .filter_map(|p| p["id"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        listed,
        vec![mine.to_string()],
        "the granted manager sees only their own project's proposal"
    );

    // Both ids in one call: the one they hold is decided, the other is refused, not applied.
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/change_proposals/decide",
        &json!({ "ids": [mine, theirs], "decision": "reject" }),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "decide ({status}): {body}");
    assert_eq!(body["rejected"], 1, "{body}");
    assert_eq!(
        body["refused"][0][0].as_str(),
        Some(theirs.to_string().as_str()),
        "the out-of-scope id is named as refused: {body}"
    );
    assert_eq!(status_of(&db, mine).await, "rejected");
    assert_eq!(
        status_of(&db, theirs).await,
        "pending",
        "another project's proposal is untouched"
    );

    // Granted that project too, the same manager decides it.
    crate::common::keycloak::grant_project(&db, &sub, PROJECT_B).await;
    state.grants_cache.invalidate_all();
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/change_proposals/decide",
        &json!({ "ids": [theirs], "decision": "reject" }),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "decide in scope ({status}): {body}");
    assert_eq!(status_of(&db, theirs).await, "rejected");

    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: a grab reading corrected by a hand-picked standard curve; the source changes the raw
/// value, the diff proposes it, and a manager accepts.
///
/// Expected behaviour: the served value is the new raw number put back through the curve the row
/// names. The decision trigger nulls `calibrated_value` whenever a correction names `raw_value`, so
/// an accept that does not recompose leaves the uncorrected raw number being served until the next
/// janitor sweep, which then records the move as a curve drift rather than as this decision.
#[tokio::test]
#[serial]
async fn accepting_a_correction_reapplies_the_curve_the_reading_names() {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, is_active, source_system, source_key) \
             VALUES ('{sensor}', 'Curve instrument', true, 'curvesrc', 'curvesrc:1')"
        ),
    )
    .await;
    // value = 2 * raw + 1
    let curve = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, name, slope, intercept) \
             VALUES ('{curve}', '{sensor}', 'Lab curve', 2.0, 1.0)"
        ),
    )
    .await;

    let stream = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, site_parameter_id, sensor_id, is_active) \
             VALUES ('{stream}', 'curvesrc', 'curve-key', '{PARAM_S1_TEMP_ID}', '{sensor}', true)"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings \
                 (stream_id, time, replicate_index, raw_value, calibrated_value, site_id, \
                  parameter_id, sensor_id, standard_curve_id, measurement_type) \
             VALUES ('{stream}', '{AT}', 0, 10.0, 21.0, \
                     (SELECT site_id FROM site_parameters WHERE id = '{PARAM_S1_TEMP_ID}'), \
                     '{GLOBAL_PARAM_TEMP_ID}', '{sensor}', '{curve}', 'spot')"
        ),
    )
    .await;

    let proposal = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reading_change_proposals \
                 (id, stream_id, time, replicate_index, proposed_raw_value, stored_raw_value, \
                  proposed_standard_curve_id, stored_standard_curve_id) \
             VALUES ('{proposal}', '{stream}', '{AT}', 0, 11.5, 10.0, '{curve}', '{curve}')"
        ),
    )
    .await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/sync/change_proposals/decide",
        &json!({ "ids": [proposal], "decision": "accept" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "accept ({status}): {body}");
    assert_eq!(status_of(&db, proposal).await, "accepted");

    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT raw_value, calibrated_value FROM readings \
                 WHERE stream_id = '{stream}' AND time = '{AT}' AND replicate_index = 0"
            ),
        ))
        .await
        .unwrap()
        .expect("the reading");
    let raw: f64 = row.try_get("", "raw_value").unwrap();
    let calibrated: Option<f64> = row.try_get("", "calibrated_value").unwrap();
    assert!((raw - 11.5).abs() < 1e-9, "the correction is the new raw: {raw}");
    // 2 * 11.5 + 1
    assert_eq!(
        calibrated,
        Some(24.0),
        "the served value is the correction put back through the row's own curve"
    );

    crate::common::cleanup_test_db(&db).await;
}
