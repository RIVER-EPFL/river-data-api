//! Decommissioning a calculation (Q272): an Administrator stops it at every site and the
//! decommission records who, when and why. Nothing it computed is withdrawn, and it is not
//! switched back on until a recommission (Q279). The decommission frees its name (Q300).

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::keycloak as kc;

const TOOL: &str = "decommission_probe";

async fn install(db: &DatabaseConnection) -> String {
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
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM tool_scripts WHERE name = $1",
            [TOOL.into()],
        ))
        .await
        .expect("query")
        .expect("the probe");
    row.try_get::<uuid::Uuid>("", "id").expect("id").to_string()
}

/// A value the probe computed at a visit, with the run that produced it, pinned to the probe's
/// version as every run is.
async fn computed_value(db: &DatabaseConnection) -> uuid::Uuid {
    let stream_id = uuid::Uuid::new_v4();
    let run_id = uuid::Uuid::new_v4();
    for sql in [
        format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active) \
             VALUES ('{stream_id}', 'grab_sample', '{stream_id}', true)"
        ),
        format!(
            "INSERT INTO tool_runs (id, tool_name, tool_version, inputs, constants, curves, outputs, created_by) \
             SELECT '{run_id}', '{TOOL}', jsonb_build_object('script_version_id', v.id, 'content_hash', v.content_hash), \
                    '{{\"x\": 2}}', '{{}}', '[]', '{{\"doubled\": 4}}', 'test' \
             FROM tool_script_versions v JOIN tool_scripts s ON s.id = v.tool_script_id \
             WHERE s.name = '{TOOL}'"
        ),
        format!(
            "INSERT INTO readings (stream_id, parameter_id, time, replicate_index, raw_value, \
                 measurement_type, provenance_kind, provenance) \
             VALUES ('{stream_id}', '{}', '2026-06-01T10:00:00Z', 0, 4.0, 'spot', 'tool_run', \
                 '{{\"run_id\": \"{run_id}\"}}')",
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    ] {
        crate::common::exec(db, &sql).await;
    }
    stream_id
}

async fn served(db: &DatabaseConnection, stream_id: uuid::Uuid) -> (f64, bool, serde_json::Value) {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT raw_value, withdrawn_at IS NOT NULL AS withdrawn, provenance \
                 FROM readings WHERE stream_id = '{stream_id}'"
            ),
        ))
        .await
        .expect("query")
        .expect("the computed value");
    (
        row.try_get("", "raw_value").expect("raw_value"),
        row.try_get("", "withdrawn").expect("withdrawn"),
        row.try_get("", "provenance").expect("provenance"),
    )
}

async fn count(db: &DatabaseConnection, table: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT count(*) AS count FROM {table}"),
    ))
    .await
    .expect("query")
    .expect("a count")
    .try_get("", "count")
    .expect("count")
}

async fn listed_in_calculation_sites(app: &axum::Router, admin: &str) -> bool {
    let (status, body) =
        crate::common::get_json_with_token(app, "/api/calculations/sites", admin).await;
    assert_eq!(status, 200, "calculation sites: {body}");
    body.as_array()
        .expect("a list")
        .iter()
        .any(|c| c["calculation"] == TOOL)
}

/// Scenario: an Administrator decommissions a calculation that has already computed a value.
///
/// Expected behaviour: it leaves the calculation set and a run by its old name is refused naming
/// the decommission; the record names the administrator, the instant and the reason, and the name
/// is freed; the value it computed is served as it was, and no ledger row or hold is written. A
/// second decommission and a run under the name it now holds are both refused naming the
/// decommission.
#[tokio::test]
#[serial]
async fn an_administrator_decommissions_a_calculation_and_its_values_stand() {
    if !crate::common::profile::Service::Keycloak
        .require("an_administrator_decommissions_a_calculation_and_its_values_stand")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    for sql in [
        format!("UPDATE tool_scripts SET active_version_id = NULL WHERE name LIKE '{TOOL}%'"),
        format!("DELETE FROM tool_scripts WHERE name LIKE '{TOOL}%'"),
    ] {
        crate::common::exec(&db, &sql).await;
    }
    let id = install(&db).await;
    let stream_id = computed_value(&db).await;
    let before = served(&db, stream_id).await;
    let decisions = count(&db, "reading_decisions").await;
    let holds = count(&db, "replicate_audit_holds").await;

    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::member_jwt("scriptadmin", "scriptadmin", "riverdata-admin").await;
    assert!(listed_in_calculation_sites(&app, &admin).await);

    let path = format!("/api/tool_scripts/{id}/decommission");
    let (status, body) =
        crate::common::post_json_parse_with_token(&app, &path, &json!({ "reason": "  " }), &admin)
            .await;
    assert_eq!(status, 400, "a blank reason is refused: {body}");

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        &path,
        &json!({ "reason": "Replaced by the corrected pCO2 formula" }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "decommissioned: {body}");
    assert!(body["decommissioned_at"].is_string(), "{body}");
    assert_eq!(
        body["decommission_reason"],
        "Replaced by the corrected pCO2 formula"
    );
    assert!(
        body["decommissioned_by"]
            .as_str()
            .is_some_and(|by| by.contains("scriptadmin")),
        "the decommission names who did it: {body}"
    );
    let today = chrono::Utc::now().format("%Y%m%d");
    assert_eq!(
        body["name"],
        format!("{TOOL}_decommissioned_{today}"),
        "the decommission frees the name: {body}"
    );

    assert!(
        !listed_in_calculation_sites(&app, &admin).await,
        "a decommissioned calculation applies at no site"
    );
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tools/{TOOL}/calculate"),
        &json!({ "x": 2 }),
        &admin,
    )
    .await;
    assert_eq!(status, 409, "a run by name is refused: {body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("decommissioned")),
        "the refusal of the freed name names the decommission: {body}"
    );

    assert_eq!(
        served(&db, stream_id).await,
        before,
        "the computed value stands"
    );
    let (status, record) = crate::common::get_json_with_token(
        &app,
        &format!("/api/readings/provenance?stream_id={stream_id}&time=2026-06-01T10:00:00Z"),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "the value's provenance: {record}");
    let decommissioned = &record["records"][0]["computation"]["decommissioned"];
    assert_eq!(
        decommissioned["reason"], "Replaced by the corrected pCO2 formula",
        "the value's provenance says its calculation was decommissioned: {record}"
    );
    assert!(decommissioned["at"].is_string(), "{record}");
    assert!(
        decommissioned["by"]
            .as_str()
            .is_some_and(|by| by.contains("scriptadmin")),
        "{record}"
    );
    assert_eq!(
        count(&db, "reading_decisions").await,
        decisions,
        "no ledger row"
    );
    assert_eq!(count(&db, "replicate_audit_holds").await, holds, "no hold");

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        &path,
        &json!({ "reason": "again" }),
        &admin,
    )
    .await;
    assert_eq!(status, 409, "a second decommission is refused: {body}");

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tools/{TOOL}_decommissioned_{today}/calculate"),
        &json!({}),
        &admin,
    )
    .await;
    assert_eq!(status, 409, "a run under its new name is refused: {body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("decommissioned")),
        "the refusal names the decommission: {body}"
    );
}

/// Only an Administrator decommissions: a manager and a `write_metadata` token are refused, and
/// the calculation stays in the set.
#[tokio::test]
#[serial]
async fn only_an_administrator_decommissions() {
    if !crate::common::profile::Service::Keycloak
        .require("only_an_administrator_decommissions")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    for sql in [
        format!("UPDATE tool_scripts SET active_version_id = NULL WHERE name LIKE '{TOOL}%'"),
        format!("DELETE FROM tool_scripts WHERE name LIKE '{TOOL}%'"),
    ] {
        crate::common::exec(&db, &sql).await;
    }
    let id = install(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let manager = kc::member_jwt("scriptmanager", "scriptmanager", "riverdata-manager").await;

    let path = format!("/api/tool_scripts/{id}/decommission");
    let reason = json!({ "reason": "not theirs to stop" });
    for (caller, credential) in [("a manager", &manager), ("a write_metadata token", &token)] {
        let (status, body) =
            crate::common::post_json_with_token(&app, &path, &reason, credential).await;
        assert_eq!(status, 403, "{caller} is refused: {body}");
    }
    let admin = kc::member_jwt("scriptadmin", "scriptadmin", "riverdata-admin").await;
    assert!(
        listed_in_calculation_sites(&app, &admin).await,
        "and it still applies"
    );
}

/// Scenario: a calculation is decommissioned, a new calculation takes its freed name, the first is
/// recommissioned, the second is decommissioned in turn and the first recommissioned again.
///
/// Expected behaviour: a recommission puts the calculation back in the calculation set and records
/// who, when and why; it gets its name back only while no other calculation holds it; the history lists every
/// decommission and recommission, newest first, under the name held before each.
#[tokio::test]
#[serial]
async fn a_recommission_brings_a_calculation_back_under_the_name_still_free() {
    if !crate::common::profile::Service::Keycloak
        .require("a_recommission_brings_a_calculation_back_under_the_name_still_free")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    for sql in [
        format!("UPDATE tool_scripts SET active_version_id = NULL WHERE name LIKE '{TOOL}%'"),
        format!("DELETE FROM tool_scripts WHERE name LIKE '{TOOL}%'"),
    ] {
        crate::common::exec(&db, &sql).await;
    }
    let first = install(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::member_jwt("scriptadmin", "scriptadmin", "riverdata-admin").await;
    let post = |path: String, reason: &'static str| {
        let (app, admin) = (&app, &admin);
        async move {
            crate::common::post_json_parse_with_token(
                app,
                &path,
                &json!({ "reason": reason }),
                admin,
            )
            .await
        }
    };
    let today = chrono::Utc::now().format("%Y%m%d").to_string();

    let (status, body) = post(format!("/api/tool_scripts/{first}/recommission"), "why not").await;
    assert_eq!(
        status, 409,
        "a live calculation is not recommissioned: {body}"
    );

    let (status, body) = post(format!("/api/tool_scripts/{first}/decommission"), "a slip").await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(format!("/api/tool_scripts/{first}/recommission"), "  ").await;
    assert_eq!(status, 400, "a blank reason is refused: {body}");
    let (status, body) = post(
        format!("/api/tool_scripts/{first}/recommission"),
        "decommissioned by mistake",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["name_restored"], true, "{body}");
    assert_eq!(body["calculation"]["name"], TOOL, "{body}");
    assert!(body["calculation"]["decommissioned_at"].is_null(), "{body}");
    assert!(
        listed_in_calculation_sites(&app, &admin).await,
        "it is back in the calculation set: {body}"
    );

    let (status, body) = post(
        format!("/api/tool_scripts/{first}/decommission"),
        "replaced",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, second) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tool_scripts",
        &json!({ "name": TOOL, "label": "Probe again" }),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "the freed name is taken by a new calculation: {second}"
    );
    let second = second["id"].as_str().expect("an id").to_string();

    let (status, body) = post(
        format!("/api/tool_scripts/{first}/recommission"),
        "needed after all",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["name_restored"], false, "{body}");
    assert_eq!(
        body["calculation"]["name"],
        format!("{TOOL}_decommissioned_{today}"),
        "the name another calculation holds stays with it: {body}"
    );

    let (status, body) = post(format!("/api/tool_scripts/{second}/decommission"), "a slip").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["name"],
        format!("{TOOL}_decommissioned_{today}_2"),
        "a second freed name the same day is counted on: {body}"
    );

    let (status, history) = crate::common::get_json_with_token(
        &app,
        &format!("/api/tool_scripts/{first}/commissions"),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{history}");
    let events: Vec<(String, String, String)> = history
        .as_array()
        .expect("a history")
        .iter()
        .map(|h| {
            (
                h["event"].as_str().unwrap().to_string(),
                h["name"].as_str().unwrap().to_string(),
                h["reason"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let freed = format!("{TOOL}_decommissioned_{today}");
    assert_eq!(
        events,
        vec![
            ("recommissioned".into(), freed, "needed after all".into()),
            ("decommissioned".into(), TOOL.into(), "replaced".into()),
            (
                "recommissioned".into(),
                format!("{TOOL}_decommissioned_{today}"),
                "decommissioned by mistake".into()
            ),
            ("decommissioned".into(), TOOL.into(), "a slip".into()),
        ],
        "{history}"
    );
    assert!(
        history[0]["actor"]
            .as_str()
            .is_some_and(|by| by.contains("scriptadmin")),
        "{history}"
    );
}
