//! S2, collection event with chained tools (PLAN.md story catalog).
//!
//! Scenario: a member stages a visit, runs one tool and saves it, and a second tool's
//! `event_inputs` resolve from the first tool's saved outputs at the same (site, collected_at).
//! The missing/stale audit reports a third tool that never ran although its input now exists, the
//! chain executor recomputes the event on demand and fills it, and the trigger statistics are
//! correct throughout.

use serde_json::json;
use serial_test::serial;

use crate::common::e2e;
use crate::common::keycloak as kc;

const EVENT_TIME: &str = "2025-06-15T09:00:00Z";

/// The three chained tools: A is typed entry, B reads A's saved output at the event, C reads B's.
async fn author_chain(app: &axum::Router, admin: &str) {
    e2e::author_tool(
        app,
        admin,
        "chain_a",
        "tool <- function(inputs, constants, curves) list(out_a = inputs$a * 2)",
        json!({
            "label": "Chain A",
            "params": [{ "name": "a", "label": "A", "kind": "number", "required": true }],
            "outputs": [{ "key": "out_a", "label": "PA", "suggested_parameter_code": "ChainPA" }],
        }),
        json!({ "name": "doubles", "inputs": { "a": 2.0 }, "expected": { "out_a": 4.0 } }),
    )
    .await;
    e2e::author_tool(
        app,
        admin,
        "chain_b",
        "tool <- function(inputs, constants, curves) list(out_b = inputs$pa + 5)",
        json!({
            "label": "Chain B",
            "params": [{ "name": "pa", "label": "PA", "kind": "number", "required": true }],
            "event_inputs": [{ "param": "pa", "parameter_code": "ChainPA" }],
            "outputs": [{ "key": "out_b", "label": "PB", "suggested_parameter_code": "ChainPB" }],
        }),
        json!({ "name": "adds", "inputs": { "pa": 1.0 }, "expected": { "out_b": 6.0 } }),
    )
    .await;
    e2e::author_tool(
        app,
        admin,
        "chain_c",
        "tool <- function(inputs, constants, curves) list(out_c = inputs$pb * 10)",
        json!({
            "label": "Chain C",
            "params": [{ "name": "pb", "label": "PB", "kind": "number", "required": true }],
            "event_inputs": [{ "param": "pb", "parameter_code": "ChainPB" }],
            "outputs": [{ "key": "out_c", "label": "PC", "suggested_parameter_code": "ChainPC" }],
        }),
        json!({ "name": "tens", "inputs": { "pb": 3.0 }, "expected": { "out_c": 30.0 } }),
    )
    .await;
}

#[tokio::test]
#[serial]
async fn two_tools_share_an_event_and_the_audit_and_executor_close_the_gap() {
    if !kc::require_keycloak_or_skip(
        "two_tools_share_an_event_and_the_audit_and_executor_close_the_gap",
    )
    .await
    {
        return;
    }
    if !crate::common::tools_runner::require_runner_or_skip(
        "two_tools_share_an_event_and_the_audit_and_executor_close_the_gap",
    )
    .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    // The chain tools are this story's fixtures; the seeded portal tools are reference data and
    // survive cleanup, so the story removes its own from a prior run.
    crate::common::exec(
        &db,
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name LIKE 'chain_%'",
    )
    .await;
    crate::common::exec(
        &db,
        "DELETE FROM tool_script_activations WHERE tool_script_id IN \
         (SELECT id FROM tool_scripts WHERE name LIKE 'chain_%')",
    )
    .await;
    crate::common::exec(
        &db,
        "DELETE FROM tool_script_versions WHERE tool_script_id IN \
         (SELECT id FROM tool_scripts WHERE name LIKE 'chain_%')",
    )
    .await;
    crate::common::exec(&db, "DELETE FROM tool_scripts WHERE name LIKE 'chain_%'").await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let project_id = e2e::create_project(&app, &admin, "Chain Project", "chainp", false).await;
    let site_id = e2e::create_site(&app, &admin, &project_id, "Chain Site", "chains").await;
    let pa = e2e::create_parameter(&app, &admin, "ChainPA", "Chain PA", "ppb").await;
    let pb = e2e::create_parameter(&app, &admin, "ChainPB", "Chain PB", "ppb").await;
    let pc = e2e::create_parameter(&app, &admin, "ChainPC", "Chain PC", "ppb").await;
    author_chain(&app, &admin).await;

    kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    kc::grant_project(&db, &kc::keycloak_user_id("river1").await, &project_id).await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;

    // Stage the visit: the portal's New Entry.
    let (status, event) = crate::common::post_json_parse_with_token(
        &app,
        "/api/collection_events",
        &json!({ "site_id": site_id, "collected_at": EVENT_TIME }),
        &river,
    )
    .await;
    assert!((200..300).contains(&status), "stage ({status}): {event}");
    let event_id = e2e::id_of(&event);

    // Tool A: typed entry, saved at the event. Auto-provisioning mints the ChainPA slot.
    let (status, a) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/chain_a/calculate",
        &json!({ "a": 21.0, "site_id": site_id, "collected_at": EVENT_TIME }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "calculate A ({status}): {a}");
    assert_eq!(a["results"]["out_a"], 42.0);
    let (status, saved) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": site_id,
            "tool_run_id": a["run_id"],
            "readings": [{ "parameter_id": pa, "value": 42.0, "time": EVENT_TIME, "output": "out_a" }],
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "save A: {saved}");

    // Tool B: nothing typed — its input resolves from A's saved output at the shared event.
    let (status, b) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/chain_b/calculate",
        &json!({ "site_id": site_id, "collected_at": EVENT_TIME }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "calculate B ({status}): {b}");
    assert_eq!(b["results"]["out_b"], 47.0);
    assert_eq!(
        b["event_inputs"][0]["parameter_code"], "ChainPA",
        "the resolution is recorded: {b}"
    );
    assert_eq!(b["event_inputs"][0]["value"], 42.0);
    let (status, saved) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": site_id,
            "tool_run_id": b["run_id"],
            "readings": [{ "parameter_id": b["event_inputs"][0]["parameter_id"], "value": 0.0,
                            "time": EVENT_TIME, "output": "out_b" }],
        }),
        &river,
    )
    .await;
    // A wrong parameter/value pairing is refused; save out_b properly.
    assert_eq!(status, 400, "a value the run did not produce is refused: {saved}");
    let (status, saved) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": site_id,
            "tool_run_id": b["run_id"],
            "readings": [{ "parameter_id": pb, "value": 47.0, "time": EVENT_TIME, "output": "out_b" }],
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "save B: {saved}");

    // The audit reports the third tool's absent outputs: its input (ChainPB) now exists.
    let (status, audit) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/event_audit",
        &json!({ "site_id": site_id }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "audit enqueue: {audit}");
    let job_id = audit["job_id"].as_str().expect("job id").to_string();
    let outcome = e2e::poll_job(&app, &admin, &job_id, 60).await;
    assert_eq!(outcome, "completed", "audit job");

    let (status, findings) = crate::common::get_json_with_token(
        &app,
        &format!("/api/actions/event_audit_findings?site_id={site_id}"),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{findings}");
    let missing: Vec<&serde_json::Value> = findings
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["kind"] == "missing_output" && f["tool"] == "chain_c")
        .collect();
    assert_eq!(missing.len(), 1, "chain_c's absent output is reported: {findings}");
    assert_eq!(missing[0]["parameter_id"].as_str().unwrap(), pc);

    // The executor recomputes the event on demand and fills the gap, in dependency order.
    let (status, recompute) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/collection_events/{event_id}/recompute"),
        &json!({}),
        &river,
    )
    .await;
    assert_eq!(status, 200, "recompute enqueue: {recompute}");
    let job_id = recompute["job_id"].as_str().expect("job id").to_string();
    let outcome = e2e::poll_job(&app, &admin, &job_id, 60).await;
    assert_eq!(outcome, "completed", "recompute job");

    // ChainPC = (42 + 5) * 10, with trigger statistics and a chain-run blob.
    let row = {
        use sea_orm::ConnectionTrait;
        db.query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT s.mean, s.n, s.provenance ->> 'tool' AS tool, \
                        s.provenance ->> 'source' AS source \
                 FROM samples s WHERE s.site_id = '{site_id}' AND s.parameter_id = '{pc}'"
            ),
        ))
        .await
        .unwrap()
        .expect("the executor formed the ChainPC sample")
    };
    assert_eq!(row.try_get::<Option<f64>>("", "mean").unwrap(), Some(470.0));
    assert_eq!(row.try_get::<i32>("", "n").unwrap(), 1);
    assert_eq!(row.try_get::<String>("", "tool").unwrap(), "chain_c");
    assert_eq!(row.try_get::<String>("", "source").unwrap(), "tool_run");

    // The readings carry the event, and a fresh audit supersedes the finding.
    let attached = {
        use sea_orm::ConnectionTrait;
        db.query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COUNT(*)::bigint AS n FROM readings \
                 WHERE collection_event_id = '{event_id}' AND parameter_id = '{pc}'"
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "n")
        .unwrap()
    };
    assert_eq!(attached, 1);

    let (status, audit) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/event_audit",
        &json!({ "site_id": site_id }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{audit}");
    let job_id = audit["job_id"].as_str().expect("job id").to_string();
    assert_eq!(e2e::poll_job(&app, &admin, &job_id, 60).await, "completed");
    let (status, findings) = crate::common::get_json_with_token(
        &app,
        &format!("/api/actions/event_audit_findings?site_id={site_id}"),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{findings}");
    assert!(
        findings.as_array().unwrap().is_empty(),
        "the filled event has no open findings: {findings}"
    );
}

/// Expected behaviour: correcting an upstream value makes every downstream output demonstrably
/// stale — the audit recomputes each saved output under its pinned version with the event's
/// current values and reports the disagreement — and the chain executor converges the event,
/// after which the audit finds nothing.
#[tokio::test]
#[serial]
async fn an_upstream_correction_surfaces_as_stale_and_recompute_converges() {
    use sea_orm::ConnectionTrait;
    if !kc::require_keycloak_or_skip(
        "an_upstream_correction_surfaces_as_stale_and_recompute_converges",
    )
    .await
    {
        return;
    }
    if !crate::common::tools_runner::require_runner_or_skip(
        "an_upstream_correction_surfaces_as_stale_and_recompute_converges",
    )
    .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::exec(
        &db,
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name LIKE 'chain_%'",
    )
    .await;
    crate::common::exec(
        &db,
        "DELETE FROM tool_script_activations WHERE tool_script_id IN \
         (SELECT id FROM tool_scripts WHERE name LIKE 'chain_%')",
    )
    .await;
    crate::common::exec(
        &db,
        "DELETE FROM tool_script_versions WHERE tool_script_id IN \
         (SELECT id FROM tool_scripts WHERE name LIKE 'chain_%')",
    )
    .await;
    crate::common::exec(&db, "DELETE FROM tool_scripts WHERE name LIKE 'chain_%'").await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let project_id = e2e::create_project(&app, &admin, "Stale Project", "stalep", false).await;
    let site_id = e2e::create_site(&app, &admin, &project_id, "Stale Site", "stales").await;
    let pa = e2e::create_parameter(&app, &admin, "ChainPA", "Chain PA", "ppb").await;
    let pb = e2e::create_parameter(&app, &admin, "ChainPB", "Chain PB", "ppb").await;
    let pc = e2e::create_parameter(&app, &admin, "ChainPC", "Chain PC", "ppb").await;
    author_chain(&app, &admin).await;

    kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    kc::grant_project(&db, &kc::keycloak_user_id("river1").await, &project_id).await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;

    let save = |run: serde_json::Value, param: String, value: f64, output: &'static str,
                replace: bool| {
        let app = app.clone();
        let river = river.clone();
        let site_id = site_id.clone();
        async move {
            let mut body = json!({
                "site_id": site_id,
                "tool_run_id": run["run_id"],
                "readings": [{ "parameter_id": param, "value": value,
                                "time": EVENT_TIME, "output": output }],
            });
            if replace {
                body["mode"] = json!("replace");
            }
            let (status, resp) =
                crate::common::post_json_with_token(&app, "/api/grab_samples", &body, &river)
                    .await;
            assert_eq!(status, 200, "save {output}: {resp}");
        }
    };

    // The initial chain: A(21) -> 42, B resolves it -> 47, C resolves that -> 470.
    let (status, a) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/chain_a/calculate",
        &json!({ "a": 21.0, "site_id": site_id, "collected_at": EVENT_TIME }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{a}");
    save(a, pa.clone(), 42.0, "out_a", false).await;
    let (status, b) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/chain_b/calculate",
        &json!({ "site_id": site_id, "collected_at": EVENT_TIME }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{b}");
    save(b, pb.clone(), 47.0, "out_b", false).await;
    let (status, c) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/chain_c/calculate",
        &json!({ "site_id": site_id, "collected_at": EVENT_TIME }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{c}");
    save(c, pc.clone(), 470.0, "out_c", false).await;

    // The upstream correction: A's input was mistyped; the corrected run replaces PA with 50.
    let (status, a2) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/chain_a/calculate",
        &json!({ "a": 25.0, "site_id": site_id, "collected_at": EVENT_TIME }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{a2}");
    save(a2, pa.clone(), 50.0, "out_a", true).await;

    // The audit recomputes B and C under their pinned versions with the corrected event values
    // and reports both stale. Nothing is written by the auditor.
    let (status, audit) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/event_audit",
        &json!({ "site_id": site_id }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{audit}");
    let job_id = audit["job_id"].as_str().expect("job id").to_string();
    assert_eq!(e2e::poll_job(&app, &admin, &job_id, 60).await, "completed");
    let (status, findings) = crate::common::get_json_with_token(
        &app,
        &format!("/api/actions/event_audit_findings?site_id={site_id}"),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{findings}");
    let stale: Vec<(&str, f64)> = findings
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["kind"] == "stale_output")
        .map(|f| {
            (
                f["tool"].as_str().unwrap(),
                f["expected"]["value"].as_f64().unwrap(),
            )
        })
        .collect();
    assert!(
        stale.contains(&("chain_b", 55.0)),
        "B is stale against the corrected upstream: {findings}"
    );
    assert_eq!(
        crate::common::e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint FROM readings \
                 WHERE site_id = '{site_id}' AND parameter_id = '{pb}' \
                   AND COALESCE(calibrated_value, raw_value) = 47.0"
            ),
        )
        .await,
        1,
        "the auditor reported and wrote nothing"
    );

    // The executor converges the whole event; a fresh audit finds nothing open.
    let event_id = db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT id::text AS id FROM collection_events \
                 WHERE site_id = '{site_id}' AND collected_at = '{EVENT_TIME}'"
            ),
        ))
        .await
        .unwrap()
        .expect("the saves attached a collection event")
        .try_get::<String>("", "id")
        .unwrap();
    let (status, recompute) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/collection_events/{event_id}/recompute"),
        &json!({}),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{recompute}");
    let job_id = recompute["job_id"].as_str().expect("job id").to_string();
    assert_eq!(e2e::poll_job(&app, &admin, &job_id, 60).await, "completed");

    for (param, expected) in [(&pb, 55.0), (&pc, 550.0)] {
        let mean = db
            .query_one(sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT mean FROM samples WHERE site_id = '{site_id}' \
                     AND parameter_id = '{param}'"
                ),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<Option<f64>>("", "mean")
            .unwrap();
        assert_eq!(mean, Some(expected), "converged value for {param}");
    }

    let (status, audit) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/event_audit",
        &json!({ "site_id": site_id }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{audit}");
    let job_id = audit["job_id"].as_str().expect("job id").to_string();
    assert_eq!(e2e::poll_job(&app, &admin, &job_id, 60).await, "completed");
    let (status, findings) = crate::common::get_json_with_token(
        &app,
        &format!("/api/actions/event_audit_findings?site_id={site_id}"),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{findings}");
    assert!(
        findings.as_array().unwrap().is_empty(),
        "the converged event has no open findings: {findings}"
    );
}
