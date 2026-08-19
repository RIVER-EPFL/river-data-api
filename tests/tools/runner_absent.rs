//! What the API does when the tool runner is not there.
//!
//! Deterministic without touching any container: the config points at a port nothing listens on,
//! or at nothing at all. Both are the states an operator meets when the sidecar is down or was
//! never configured.

use river_db::common::AppState;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::keycloak as kc;

const PROBE: &str = "probe_runner_absent";

/// A tool whose manifest requires nothing, so a request reaches the runner instead of being
/// refused by validation first.
async fn install_probe_tool(db: &DatabaseConnection) {
    for sql in [
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name = $1",
        "DELETE FROM tool_scripts WHERE name = $1",
    ] {
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            [PROBE.into()],
        ))
        .await
        .expect("probe tool cleared");
    }
    let manifest = json!({
        "label": "Runner absent probe",
        "params": [],
        "outputs": [ { "key": "ok", "label": "OK", "per_replicate": false } ],
    });
    for statement in [
        Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO tool_scripts (name, label, created_by) VALUES ($1, 'Probe', 'test')",
            [PROBE.into()],
        ),
        Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"INSERT INTO tool_script_versions
                  (tool_script_id, version_no, script, entry_function, manifest, test_cases,
                   content_hash, created_by, validated_at)
              SELECT s.id, 1, $2, 'tool', $3::jsonb, '{}'::jsonb, md5($2), 'test', now()
              FROM tool_scripts s WHERE s.name = $1",
            [
                PROBE.into(),
                "tool <- function(inputs, constants, curves) list(ok = 1)".into(),
                manifest.to_string().into(),
            ],
        ),
        Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE tool_scripts s SET active_version_id = v.id
              FROM tool_script_versions v
              WHERE v.tool_script_id = s.id AND s.name = $1",
            [PROBE.into()],
        ),
    ] {
        db.execute(statement).await.expect("probe tool installed");
    }
}

/// A port nothing listens on: bound to learn a free one, then released, so the connection is
/// refused immediately rather than hanging until the request timeout.
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    listener.local_addr().expect("the bound address").port()
}

async fn app_with_runner(db: DatabaseConnection, runner_url: Option<String>) -> axum::Router {
    let mut config = crate::common::test_config();
    config.tools_runner_url = runner_url;
    config.tools_runner_timeout_seconds = 5;
    river_db::routes::build_router(AppState::new(db, config, None))
}

async fn setup(runner_url: Option<String>) -> (axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    install_probe_tool(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    (app_with_runner(db, runner_url).await, token)
}

async fn assert_calculate_is_unavailable(app: &axum::Router, token: &str) {
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        &format!("/api/tools/{PROBE}/calculate"),
        &json!({}),
        token,
    )
    .await;
    assert_eq!(status, 503, "the runner is absent: {body}");
    let message = body.to_string().to_lowercase();
    assert!(
        message.contains("runner"),
        "the message names the runner: {body}"
    );
}

#[tokio::test]
#[serial]
async fn an_unreachable_runner_answers_503_and_leaves_the_rest_of_the_api_alone() {
    let (app, token) = setup(Some(format!("http://127.0.0.1:{}/ocpu", closed_port()))).await;

    assert_calculate_is_unavailable(&app, &token).await;

    for uri in ["/api/tools", "/api/sites", "/api/parameters"] {
        let (status, body) = crate::common::get_json_with_token(&app, uri, &token).await;
        assert_eq!(status, 200, "{uri} does not depend on the runner: {body}");
    }
}

#[tokio::test]
#[serial]
async fn an_unconfigured_runner_answers_503_and_says_so() {
    let (app, token) = setup(None).await;

    assert_calculate_is_unavailable(&app, &token).await;
}

/// Expected behaviour: a manifest is refused by rules the API owns, so the refusal that names the
/// offending field survives the sidecar being down. Only the script lint, which is read off the
/// runner, may report the outage.
#[tokio::test]
#[serial]
async fn a_malformed_manifest_is_refused_without_the_runner() {
    if !kc::require_keycloak_or_skip("a_malformed_manifest_is_refused_without_the_runner").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = kc::build_test_app_with_keycloak_and_runner(
        db.clone(),
        Some(format!("http://127.0.0.1:{}/ocpu", closed_port())),
    )
    .await;
    kc::ensure_realm_user("manifestadmin", "manifestadmin", &["riverdata-admin"]).await;
    let admin = kc::get_keycloak_jwt("manifestadmin", "manifestadmin").await;

    let (status, script) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tool_scripts",
        &json!({ "name": "manifest_probe", "label": "Manifest probe" }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{script}");
    let sid = script["id"].as_str().expect("the script id").to_string();

    let version = |params: serde_json::Value| {
        json!({
            "script": "tool <- function(inputs, constants, curves) list(x = 1)",
            "manifest": { "label": "Manifest probe", "params": params, "outputs": [] },
        })
    };
    let post_version = async |body: serde_json::Value| {
        crate::common::post_json_parse_with_token(
            &app,
            &format!("/api/tool_scripts/{sid}/versions"),
            &body,
            &admin,
        )
        .await
    };
    let malformed = post_version(version(
        json!([{ "name": "hue", "label": "Hue", "kind": "colour" }]),
    ))
    .await;
    let well_formed = post_version(version(
        json!([{ "name": "x", "label": "X", "kind": "number" }]),
    ))
    .await;

    crate::common::exec(
        &db,
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name = 'manifest_probe'",
    )
    .await;
    crate::common::exec(
        &db,
        "DELETE FROM tool_scripts WHERE name = 'manifest_probe'",
    )
    .await;

    assert_eq!(malformed.0, 400, "unknown kind: {}", malformed.1);
    assert!(
        malformed.1["error"]
            .as_str()
            .is_some_and(|e| e.contains("colour")),
        "the refusal names the kind rather than the outage: {}",
        malformed.1
    );
    assert_eq!(
        well_formed.0, 503,
        "the lint needs the runner: {}",
        well_formed.1
    );
    assert!(
        well_formed.1.to_string().to_lowercase().contains("runner"),
        "the message names the runner: {}",
        well_formed.1
    );
}
