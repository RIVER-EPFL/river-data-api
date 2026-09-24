//! A decommissioned calculation is refused by name. The refusal names the decommission, so an
//! operator is not sent looking for a calculation that is there, and it lands before the runner,
//! so the results of a decommissioned calculation never exist to be saved. The CSV import resolves
//! the calculation through the same lookup and answers the same way.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

const TOOL: &str = "decommissioned_probe";

async fn install(db: &DatabaseConnection) {
    for statement in [
        Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO tool_scripts (name, label, created_by) VALUES ($1, 'Probe', 'test')",
            [TOOL.into()],
        ),
        Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"INSERT INTO tool_script_versions
                  (tool_script_id, version_no, script, entry_function, manifest, test_cases,
                   content_hash, created_by, validated_at)
              SELECT s.id, 1, $2, 'tool', $3::jsonb, '{}'::jsonb, md5($2), 'test', now()
              FROM tool_scripts s WHERE s.name = $1",
            [
                TOOL.into(),
                "tool <- function(inputs, constants, curves) list(doubled = 2 * inputs$x)".into(),
                json!({
                    "label": "Probe",
                    "params": [{ "name": "x", "label": "X", "kind": "number", "required": true }],
                    "outputs": [{ "key": "doubled", "label": "Doubled", "per_replicate": false }],
                })
                .to_string()
                .into(),
            ],
        ),
        Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE tool_scripts s SET active_version_id = v.id
              FROM tool_script_versions v
              WHERE v.tool_script_id = s.id AND s.name = $1",
            [TOOL.into()],
        ),
    ] {
        db.execute_raw(statement).await.expect("probe installed");
    }
}

/// Stamped as decommissioned under the name it runs by, so the lookup meets the stamp itself
/// rather than a freed name.
async fn decommission(db: &DatabaseConnection) {
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE tool_scripts SET decommissioned_at = now(), decommissioned_by = 'test',
             decommission_reason = 'retired' WHERE name = $1",
        [TOOL.into()],
    ))
    .await
    .expect("probe decommissioned");
}

async fn calculate(app: &axum::Router, token: &str) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        app,
        &format!("/api/tools/{TOOL}/calculate"),
        &json!({ "x": 2 }),
        token,
    )
    .await
}

#[tokio::test]
#[serial]
async fn a_decommissioned_calculation_is_refused_by_name() {
    let f = crate::common::seeded_app().await;
    install(&f.db).await;
    decommission(&f.db).await;

    let (status, body) = calculate(&f.app, &f.token).await;
    assert_eq!(status, 409, "a decommissioned calculation is refused: {body}");
    let message = body["error"].as_str().unwrap_or_default().to_string();
    assert!(
        message.contains(TOOL) && message.contains("decommissioned"),
        "the refusal names the calculation and the decommission: {body}"
    );

    let (status, body) = crate::common::post_json_parse_with_token(
        &f.app,
        "/api/tools/no_such_calculation/calculate",
        &json!({ "x": 2 }),
        &f.token,
    )
    .await;
    assert_eq!(
        status, 404,
        "a calculation that is not there is still absent: {body}"
    );
}

#[tokio::test]
#[serial]
async fn a_recommissioned_calculation_runs_again() {
    if !crate::common::profile::Service::ToolsRunner
        .require("a_recommissioned_calculation_runs_again")
        .await
    {
        return;
    }
    let f = crate::common::seeded_app().await;
    install(&f.db).await;
    decommission(&f.db).await;
    assert_eq!(calculate(&f.app, &f.token).await.0, 409);

    f.db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE tool_scripts SET decommissioned_at = NULL, decommissioned_by = NULL,
             decommission_reason = NULL WHERE name = $1",
        [TOOL.into()],
    ))
    .await
    .expect("probe recommissioned");

    let (status, body) = calculate(&f.app, &f.token).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["results"]["doubled"], 4.0);
}
