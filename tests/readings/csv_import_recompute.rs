//! Scenario: a plain CSV import lands a value at a manual visit that an enabled calculation reads.
//!
//! Expected behaviour: the calculation runs without anyone asking (ADR 0007), the same as when the
//! value arrives through `/readings/batch` or a grab save. The importer is a person entering visits
//! after the fact, so its events are manual and never exempt.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const AT: &str = "2025-06-15T11:00:00Z";

/// An enabled calculation whose only event input is the site's temperature parameter.
async fn install_calculation(db: &DatabaseConnection) {
    let code: String = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT code FROM parameters WHERE id = '{GLOBAL_PARAM_TEMP_ID}'"),
        ))
        .await
        .expect("code query")
        .expect("the seeded parameter")
        .try_get("", "code")
        .expect("code column");

    // The seeded portal tools are reference data and survive cleanup, so this fixture does too:
    // remove the prior run's before installing this one's.
    for sql in [
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name = 'csvrecompute'",
        "DELETE FROM tool_script_versions v USING tool_scripts s \
          WHERE v.tool_script_id = s.id AND s.name = 'csvrecompute'",
        "DELETE FROM tool_scripts WHERE name = 'csvrecompute'",
    ] {
        crate::common::exec(db, sql).await;
    }

    let manifest = serde_json::json!({
        "label": "CSV recompute",
        "params": [{ "name": "t", "label": "T", "kind": "number", "required": true }],
        "event_inputs": [{ "param": "t", "parameter_code": code }],
        "outputs": [{ "key": "out", "label": "O", "suggested_parameter_code": "CsvRecomputeOut" }],
    });
    for statement in [
        Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO tool_scripts (name, label, created_by) \
             VALUES ('csvrecompute', 'CSV recompute', 'test')"
                .to_string(),
        ),
        Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"INSERT INTO tool_script_versions
                  (tool_script_id, version_no, script, entry_function, manifest, test_cases,
                   content_hash, created_by, validated_at)
              SELECT s.id, 1, $1, 'tool', $2::jsonb, '{}'::jsonb, md5($1), 'test', now()
              FROM tool_scripts s WHERE s.name = 'csvrecompute'",
            [
                "tool <- function(inputs, constants, curves) list(out = 1)".into(),
                manifest.to_string().into(),
            ],
        ),
        Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE tool_scripts s SET active_version_id = v.id
              FROM tool_script_versions v
              WHERE v.tool_script_id = s.id AND s.name = 'csvrecompute'"
                .to_string(),
        ),
    ] {
        db.execute_raw(statement)
            .await
            .expect("calculation installed");
    }
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    crate::common::e2e::count(db, sql).await
}

/// Wait for a count query to reach `want`, then hold it to exactly that. Every row this test reads
/// is written by the import worker, so a count read once is a race with the run that writes it.
async fn await_count(db: &DatabaseConnection, sql: &str, want: i64, what: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let n = count(db, sql).await;
        if n == want {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what} (last count {n}, wanted {want})"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[serial]
async fn a_csv_import_runs_the_calculations_that_read_what_it_landed() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    install_calculation(&db).await;

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({
            "site": SITE1_ID,
            "measurement_type": "spot",
            "csv": "DateTime,DO_Temperature\n2025-06-15 11:00:00,12.5\n",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");

    // The visit row and the recompute job are two writes by the same worker run, so both are
    // waited for; asserting the second the moment the first lands fails whenever the worker is
    // between them.
    await_count(
        &db,
        &format!(
            "SELECT COUNT(*)::bigint FROM collection_events \
             WHERE site_id = '{SITE1_ID}' AND collected_at = '{AT}'"
        ),
        1,
        "the import worker never staged the visit",
    )
    .await;

    await_count(
        &db,
        &format!(
            "SELECT COUNT(*)::bigint FROM reprocessing_jobs j \
             JOIN collection_events ce ON ce.id = j.trigger_id \
             WHERE j.trigger_type = 'event_recompute' \
               AND ce.site_id = '{SITE1_ID}' AND ce.collected_at = '{AT}'"
        ),
        1,
        "the value landed at a manual visit an enabled calculation reads, so its recompute is queued",
    )
    .await;

    crate::common::cleanup_test_db(&db).await;
}
