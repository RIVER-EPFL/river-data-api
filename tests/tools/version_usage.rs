//! What each version of a calculation has already produced, which is what an author is shown
//! before choosing between leaving those values where they are and recomputing them.
//!
//! Run: cargo test --test tools version_usage -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use crate::common::keycloak as kc;
use crate::common::{GLOBAL_PARAM_DO_ID, SITE1_ID};

const NAME: &str = "usage_probe";
const AT: &str = "2025-03-04T09:00:00Z";

async fn install_two_versions(db: &DatabaseConnection) -> (String, String) {
    for sql in [
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name = 'usage_probe'",
        "DELETE FROM tool_script_versions v USING tool_scripts s \
          WHERE v.tool_script_id = s.id AND s.name = 'usage_probe'",
        "DELETE FROM tool_scripts WHERE name = 'usage_probe'",
    ] {
        crate::common::exec(db, sql).await;
    }
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO tool_scripts (name, label, created_by) VALUES ('{NAME}', '{NAME}', 'test')"
        ),
    )
    .await;
    for version_no in [1, 2] {
        db.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"INSERT INTO tool_script_versions
                  (tool_script_id, version_no, script, entry_function, manifest, test_cases,
                   content_hash, created_by, validated_at)
              SELECT s.id, $1, $2, 'tool', '{}'::jsonb, '{}'::jsonb, md5($2 || $1::text), 'test', now()
              FROM tool_scripts s WHERE s.name = 'usage_probe'",
            [
                version_no.into(),
                "tool <- function(inputs, constants, curves) list(out = 1)".into(),
            ],
        ))
        .await
        .expect("version installed");
    }
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT s.id::text AS script, v.id::text AS first \
               FROM tool_scripts s JOIN tool_script_versions v ON v.tool_script_id = s.id \
              WHERE s.name = 'usage_probe' AND v.version_no = 1"
                .to_string(),
        ))
        .await
        .expect("query")
        .expect("the installed calculation");
    (
        row.try_get::<String>("", "script").expect("script id"),
        row.try_get::<String>("", "first").expect("version id"),
    )
}

/// Two readings at one visit, computed under `version_id`.
async fn store_readings(db: &DatabaseConnection, version_id: &str) {
    let event = uuid::Uuid::new_v4();
    let stream = uuid::Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO collection_events (id, site_id, collected_at, source) \
             VALUES ('{event}', '{SITE1_ID}', '{AT}', 'manual')"
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active) \
             VALUES ('{stream}', 'test-usage', 'usage-probe', true)"
        ),
    )
    .await;
    for replicate in 0..2 {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO readings \
                     (stream_id, time, replicate_index, raw_value, site_id, parameter_id, \
                      collection_event_id, measurement_type, provenance) \
                 VALUES ('{stream}', '{AT}', {replicate}, 1.0, '{SITE1_ID}', \
                         '{GLOBAL_PARAM_DO_ID}', '{event}', 'spot', \
                         '{{\"tool_version\": {{\"script_version_id\": \"{version_id}\"}}}}'::jsonb)"
            ),
        )
        .await;
    }
}

/// Scenario: an author is about to save a correction to a calculation two versions old, and the
/// page has to say what the version standing now left behind.
///
/// Expected behaviour: every version is a row, the one the stored provenance names carries the
/// readings and the visit they belong to, and the one nothing was computed under carries zeros
/// rather than being left out.
#[tokio::test]
#[serial]
async fn each_version_reports_the_readings_and_visits_it_produced() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (script_id, first_version) = install_two_versions(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::member_jwt("usageadmin", "usageadmin", "riverdata-admin").await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/tool_scripts/{script_id}/version_usage"),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let rows = body.as_array().expect("a row per version");
    assert_eq!(rows.len(), 2, "every version is a row: {body}");
    assert_eq!(rows[0]["version_no"], 2, "newest first: {body}");
    for row in rows {
        assert_eq!(row["readings"], 0, "nothing stored yet: {body}");
        assert_eq!(row["visits"], 0, "nothing stored yet: {body}");
    }

    store_readings(&db, &first_version).await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/tool_scripts/{script_id}/version_usage"),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let rows = body.as_array().expect("a row per version");
    let first = rows
        .iter()
        .find(|r| r["version_id"] == first_version)
        .expect("the version the readings name");
    assert_eq!(first["readings"], 2, "{body}");
    assert_eq!(first["visits"], 1, "the two readings share a visit: {body}");
    let second = rows
        .iter()
        .find(|r| r["version_no"] == 2)
        .expect("the version nothing was computed under");
    assert_eq!(second["readings"], 0, "{body}");
    assert_eq!(second["visits"], 0, "{body}");
}
