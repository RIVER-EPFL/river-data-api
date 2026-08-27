//! Every active tool version's stored test cases, run through the validation path.
//!
//! The cases are the claim that the DB-stored R reproduces the legacy portal, and until now they
//! only ran when someone clicked Validate in the portal. This drives them from the database, so
//! adding a tool or a case extends the proof without touching this file: nothing here names a
//! tool, a case or a number.
//!
//! Needs the OpenCPU runner on `TOOLS_RUNNER_URL`.

use axum::extract::{Path, State};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use river_db::routes::private::tools::scripts;

struct ActiveVersion {
    name: String,
    script_id: Uuid,
    version_id: Uuid,
    cases: i64,
    seeded: bool,
}

async fn active_versions(db: &DatabaseConnection) -> Vec<ActiveVersion> {
    db.query_all(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        r"SELECT s.name, s.id AS script_id, v.id AS version_id,
                 COALESCE(jsonb_array_length(v.test_cases->'cases'), 0)::bigint AS cases,
                 (v.created_by = 'seed') AS seeded
          FROM tool_scripts s
          JOIN tool_script_versions v ON v.id = s.active_version_id
          ORDER BY s.name"
            .to_string(),
    ))
    .await
    .expect("read the active tool versions")
    .into_iter()
    .map(|row| ActiveVersion {
        name: row.try_get("", "name").expect("name"),
        script_id: row.try_get("", "script_id").expect("script_id"),
        version_id: row.try_get("", "version_id").expect("version_id"),
        cases: row.try_get("", "cases").expect("cases"),
        seeded: row.try_get("", "seeded").expect("seeded"),
    })
    .collect()
}

#[tokio::test]
#[serial]
async fn every_active_tool_version_passes_its_stored_cases() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "every_active_tool_version_passes_its_stored_cases",
    )
    .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let versions = active_versions(&db).await;
    assert!(
        versions.iter().any(|v| v.seeded),
        "the seeded tools should be active, so there is something to prove"
    );

    let mut failures: Vec<String> = Vec::new();
    let mut cases_run = 0i64;
    let mut tools_run = 0usize;
    for version in &versions {
        // A seeded tool without cases is the proof going missing; a probe tool another suite
        // left behind never had any and is not this test's subject.
        if version.cases == 0 {
            if version.seeded {
                failures.push(format!(
                    "{}: the active seeded version ships no cases",
                    version.name
                ));
            }
            continue;
        }
        cases_run += version.cases;
        tools_run += 1;

        let outcome = scripts::validate_version(
            State(state.clone()),
            Path((version.script_id, version.version_id)),
        )
        .await;
        match outcome {
            Err(e) => failures.push(format!("{}: validation failed: {e}", version.name)),
            Ok(axum::Json(response)) => {
                for case in response.cases.iter().filter(|c| !c.passed) {
                    let detail = match &case.error {
                        Some(error) => format!("script error: {error}"),
                        None => case.failures.join("; "),
                    };
                    failures.push(format!("{} / {}: {detail}", version.name, case.name));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} stored cases across {} tools did not reproduce their expected values:\n{}",
        failures.len(),
        cases_run,
        tools_run,
        failures.join("\n")
    );
}
