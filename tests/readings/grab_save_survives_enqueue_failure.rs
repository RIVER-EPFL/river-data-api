//! Scenario: a grab save commits its readings, and the recompute enqueue that follows the commit
//! fails.
//!
//! Expected behaviour: the save answers 200. The rows are committed before the tail runs, so a
//! 500 there reports a write that happened as a failed one, and the operator saves the same
//! values again.

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const AT: &str = "2025-06-15T10:00:00Z";

/// An enabled calculation whose only event input is the site's temperature parameter, so a grab
/// save of that parameter reaches the enqueue.
async fn install_calculation(db: &DatabaseConnection) {
    let code: String = crate::common::e2e::scalar(
        db,
        &format!("SELECT code AS v FROM parameters WHERE id = '{GLOBAL_PARAM_TEMP_ID}'"),
    )
    .await;
    let manifest = json!({
        "label": "Enqueue failure",
        "params": [{ "name": "t", "label": "T", "kind": "number", "required": true }],
        "event_inputs": [{ "param": "t", "parameter_code": code }],
        "outputs": [{ "key": "out", "label": "O", "suggested_parameter_code": "EnqueueFailureOut" }],
    });
    for statement in [
        Statement::from_string(
            DatabaseBackend::Postgres,
            "INSERT INTO tool_scripts (name, label, created_by) \
             VALUES ('enqueuefailure', 'Enqueue failure', 'test')"
                .to_string(),
        ),
        Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            r"INSERT INTO tool_script_versions
                  (tool_script_id, version_no, script, entry_function, manifest, test_cases,
                   content_hash, created_by, validated_at)
              SELECT s.id, 1, $1, 'tool', $2::jsonb, '{}'::jsonb, md5($1), 'test', now()
              FROM tool_scripts s WHERE s.name = 'enqueuefailure'",
            [
                "tool <- function(inputs, constants, curves) list(out = 1)".into(),
                manifest.to_string().into(),
            ],
        ),
        Statement::from_string(
            DatabaseBackend::Postgres,
            r"UPDATE tool_scripts s SET active_version_id = v.id
              FROM tool_script_versions v
              WHERE v.tool_script_id = s.id AND s.name = 'enqueuefailure'"
                .to_string(),
        ),
    ] {
        db.execute_raw(statement)
            .await
            .expect("calculation installed");
    }
}

/// Make every `reprocessing_jobs` insert fail, which is what a transient database error on the
/// enqueue looks like from the request tail.
async fn break_the_enqueue(db: &DatabaseConnection) {
    for sql in [
        "CREATE OR REPLACE FUNCTION test_refuse_job() RETURNS trigger AS \
         $$ BEGIN RAISE EXCEPTION 'enqueue refused'; END; $$ LANGUAGE plpgsql",
        "CREATE TRIGGER test_refuse_job BEFORE INSERT ON reprocessing_jobs \
         FOR EACH ROW EXECUTE FUNCTION test_refuse_job()",
    ] {
        crate::common::exec(db, sql).await;
    }
}

async fn repair_the_enqueue(db: &DatabaseConnection) {
    for sql in [
        "DROP TRIGGER IF EXISTS test_refuse_job ON reprocessing_jobs",
        "DROP FUNCTION IF EXISTS test_refuse_job()",
    ] {
        crate::common::exec(db, sql).await;
    }
}

#[tokio::test]
#[serial]
async fn a_grab_save_whose_recompute_cannot_be_queued_still_answers_the_write_it_made() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    install_calculation(&db).await;
    break_the_enqueue(&db).await;

    let (status, body) = crate::common::post_checked_grab(
        &app,
        &json!({
            "site_id": SITE1_ID,
            "readings": [
                { "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 185.2, "time": AT },
                { "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 198.7, "time": AT },
                { "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 191.4, "time": AT }
            ]
        }),
        &token,
    )
    .await;
    repair_the_enqueue(&db).await;

    assert_eq!(status, 200, "the readings are committed: {body}");
    let stored = crate::common::e2e::count(
        &db,
        &format!(
            "SELECT COUNT(*)::bigint FROM readings \
             WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}' \
               AND time = '{AT}'"
        ),
    )
    .await;
    assert_eq!(stored, 3, "all three replicates are stored");

    crate::common::cleanup_test_db(&db).await;
}
