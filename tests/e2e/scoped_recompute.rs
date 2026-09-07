//! The scoped apply (M24): after a change the reactive hook does not see (a constant, a curve, a
//! script activation, or here a calculation switched off while its input was corrected), the
//! audit reports every stale visit at a site and one site-scoped recompute repairs them all and
//! closes the findings it repaired.

use serde_json::json;
use serial_test::serial;

use crate::common::e2e;
use crate::common::keycloak as kc;

const VISITS: [&str; 2] = ["2025-06-15T09:00:00Z", "2025-06-22T09:00:00Z"];

async fn set_enabled(app: &axum::Router, admin: &str, name: &str, enabled: bool) {
    let (status, scripts) =
        crate::common::get_json_with_token(app, "/api/tool_scripts", admin).await;
    assert_eq!(status, 200, "{scripts}");
    let id = scripts
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .map(|s| s["id"].as_str().unwrap().to_string())
        .unwrap_or_else(|| panic!("{name} is listed"));
    let (status, patched) = crate::common::patch_json_with_token(
        app,
        &format!("/api/tool_scripts/{id}"),
        &json!({ "enabled": enabled }),
        admin,
    )
    .await;
    assert_eq!(status, 200, "{patched}");
}

#[tokio::test]
#[serial]
async fn a_site_scoped_recompute_repairs_every_stale_visit_and_closes_the_findings() {
    use sea_orm::ConnectionTrait;
    if !kc::require_keycloak_or_skip(
        "a_site_scoped_recompute_repairs_every_stale_visit_and_closes_the_findings",
    )
    .await
    {
        return;
    }
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_site_scoped_recompute_repairs_every_stale_visit_and_closes_the_findings",
    )
    .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::exec(
        &db,
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name LIKE 'scope_%'",
    )
    .await;
    crate::common::exec(
        &db,
        "DELETE FROM tool_script_activations WHERE tool_script_id IN \
         (SELECT id FROM tool_scripts WHERE name LIKE 'scope_%')",
    )
    .await;
    crate::common::exec(
        &db,
        "DELETE FROM tool_script_versions WHERE tool_script_id IN \
         (SELECT id FROM tool_scripts WHERE name LIKE 'scope_%')",
    )
    .await;
    crate::common::exec(&db, "DELETE FROM tool_scripts WHERE name LIKE 'scope_%'").await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let project_id = e2e::create_project(&app, &admin, "Scope Project", "scopep", false).await;
    let site_id = e2e::create_site(&app, &admin, &project_id, "Scope Site", "scopes").await;
    let other_site = e2e::create_site(&app, &admin, &project_id, "Other Site", "others").await;
    let pa = e2e::create_parameter(&app, &admin, "ScopeA", "Scope A", "ppb").await;
    let pb = e2e::create_parameter(&app, &admin, "ScopeB", "Scope B", "ppb").await;
    // A parameter belongs to one group, so both sites carry the same one.
    let group_id = e2e::declare_site_slots(
        &db,
        &app,
        &admin,
        &site_id,
        "scope_group",
        &[(pa.as_str(), "measured"), (pb.as_str(), "output")],
    )
    .await;
    let (status, applied) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sites/{other_site}/parameter_groups"),
        &json!({ "group_id": group_id }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "apply the group at the other site: {applied}");
    e2e::author_tool(
        &app,
        &admin,
        "scope_b",
        "tool <- function(inputs, constants, curves) list(out_b = inputs$a + 5)",
        json!({
            "label": "Scope B",
            "params": [{ "name": "a", "label": "A", "kind": "number", "required": true }],
            "event_inputs": [{ "param": "a", "parameter_code": "ScopeA" }],
            "outputs": [{ "key": "out_b", "label": "B", "suggested_parameter_code": "ScopeB" }],
        }),
        json!({ "name": "adds", "inputs": { "a": 1.0 }, "expected": { "out_b": 6.0 } }),
    )
    .await;

    kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    kc::grant_project(&db, &kc::keycloak_user_id("river1").await, &project_id).await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;

    let save_a = |site: String, value: f64, at: &'static str, replace: bool| {
        let app = app.clone();
        let river = river.clone();
        let pa = pa.clone();
        async move {
            let mut body = json!({
                "site_id": site,
                "readings": [{ "parameter_id": pa, "value": value, "time": at }],
            });
            if replace {
                body["mode"] = json!("replace");
            }
            let (status, resp) =
                crate::common::post_json_with_token(&app, "/api/grab_samples", &body, &river).await;
            assert_eq!(status, 200, "save ScopeA: {resp}");
        }
    };
    let served_b = |site: String, at: &'static str| {
        let db = db.clone();
        let pb = pb.clone();
        async move {
            db.query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT COALESCE(r.calibrated_value, r.raw_value) AS value FROM readings r \
                     WHERE r.site_id = '{site}' AND r.parameter_id = '{pb}' AND r.time = '{at}' \
                       AND r.withdrawn_at IS NULL ORDER BY r.replicate_index LIMIT 1"
                ),
            ))
            .await
            .unwrap()
            .and_then(|r| r.try_get::<Option<f64>>("", "value").ok().flatten())
        }
    };
    let pending_findings = |site: String| {
        let app = app.clone();
        let admin = admin.clone();
        async move { e2e::pending_event_findings(&app, &admin, &site).await }
    };

    // Both visits at the site, and one at the other site, compute B from A on the save.
    for at in VISITS {
        save_a(site_id.clone(), 10.0, at, false).await;
    }
    save_a(other_site.clone(), 10.0, VISITS[0], false).await;
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    for at in VISITS {
        assert_eq!(served_b(site_id.clone(), at).await, Some(15.0));
    }
    assert_eq!(served_b(other_site.clone(), VISITS[0]).await, Some(15.0));

    // The corrections land while the calculation is off, so every B at the site is stale.
    set_enabled(&app, &admin, "scope_b", false).await;
    for at in VISITS {
        save_a(site_id.clone(), 20.0, at, true).await;
    }
    save_a(other_site.clone(), 20.0, VISITS[0], true).await;
    set_enabled(&app, &admin, "scope_b", true).await;
    let (status, audit) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/event_audit",
        &json!({}),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{audit}");
    let job_id = audit["job_id"].as_str().expect("job id").to_string();
    assert_eq!(e2e::poll_job(&app, &admin, &job_id, 60).await, "completed");
    let stale = pending_findings(site_id.clone()).await;
    assert_eq!(stale.len(), 2, "both visits report B stale: {stale:?}");
    assert_eq!(pending_findings(other_site.clone()).await.len(), 1);

    // One site-scoped apply, held to the visits with open findings, repairs both and closes both.
    let (status, apply) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/event_recompute",
        &json!({ "site_id": site_id, "only_findings": true }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{apply}");
    let job_id = apply["job_id"].as_str().expect("job id").to_string();
    assert_eq!(e2e::poll_job(&app, &admin, &job_id, 60).await, "completed");
    for at in VISITS {
        assert_eq!(
            served_b(site_id.clone(), at).await,
            Some(25.0),
            "repaired at {at}"
        );
    }
    assert!(
        pending_findings(site_id.clone()).await.is_empty(),
        "the repaired findings closed"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COALESCE((detail -> 'counts' ->> 'events_recomputed')::bigint, 0) \
                 FROM reprocessing_jobs WHERE id = '{job_id}'"
            ),
        )
        .await,
        2
    );

    // The other site was outside the scope: still stale, still reported.
    assert_eq!(served_b(other_site.clone(), VISITS[0]).await, Some(15.0));
    assert_eq!(pending_findings(other_site.clone()).await.len(), 1);

    // A scope with nothing to repair is a completed job that recomputed nothing.
    let (status, apply) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/event_recompute",
        &json!({ "site_id": site_id, "only_findings": true }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{apply}");
    let job_id = apply["job_id"].as_str().expect("job id").to_string();
    assert_eq!(e2e::poll_job(&app, &admin, &job_id, 60).await, "completed");
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COALESCE((detail -> 'counts' ->> 'events_recomputed')::bigint, 0) \
                 FROM reprocessing_jobs WHERE id = '{job_id}'"
            ),
        )
        .await,
        0
    );

    // A range that covers only the first visit repairs only that one at the other site.
    let (status, apply) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/event_recompute",
        &json!({ "site_id": other_site, "start": "2025-06-14T00:00:00Z", "end": "2025-06-16T00:00:00Z" }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{apply}");
    let job_id = apply["job_id"].as_str().expect("job id").to_string();
    assert_eq!(e2e::poll_job(&app, &admin, &job_id, 60).await, "completed");
    assert_eq!(served_b(other_site.clone(), VISITS[0]).await, Some(25.0));
    assert!(pending_findings(other_site.clone()).await.is_empty());

    // An edit to the calculation itself. Every stored B was correct under the version that made
    // it, so judging a run only under its own version says nothing; the audit has to ask what the
    // calculation computes now.
    e2e::revise_tool(
        &app,
        &admin,
        "scope_b",
        "tool <- function(inputs, constants, curves) list(out_b = inputs$a + 7)",
        json!({
            "label": "Scope B",
            "params": [{ "name": "a", "label": "A", "kind": "number", "required": true }],
            "event_inputs": [{ "param": "a", "parameter_code": "ScopeA" }],
            "outputs": [{ "key": "out_b", "label": "B", "suggested_parameter_code": "ScopeB" }],
        }),
        json!({ "name": "adds", "inputs": { "a": 1.0 }, "expected": { "out_b": 8.0 } }),
    )
    .await;

    let (status, audit) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/event_audit",
        &json!({}),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{audit}");
    let job_id = audit["job_id"].as_str().expect("job id").to_string();
    assert_eq!(e2e::poll_job(&app, &admin, &job_id, 60).await, "completed");

    let edited = pending_findings(site_id.clone()).await;
    assert_eq!(
        edited.len(),
        2,
        "the edit leaves both visits stale and reported: {edited:?}"
    );
    assert_eq!(
        edited[0]["expected"]["reason"], "calculation",
        "the finding says the calculation moved, not the inputs: {edited:?}"
    );
    assert_eq!(
        edited[0]["expected"]["value"].as_f64(),
        Some(27.0),
        "and what it would compute now: {edited:?}"
    );
    // Reporting is all it does: no value is rewritten until somebody applies the repair.
    assert_eq!(served_b(site_id.clone(), VISITS[0]).await, Some(25.0));
}

/// Q108 end to end: an input is corrected, the calculation over it is recomputed without anyone
/// asking, the value's record names the correction that moved it, and rolling the correction back
/// puts both the value and its record where they started.
#[tokio::test]
#[serial]
async fn a_correction_cascades_is_recorded_and_is_reversible() {
    const AT: &str = "2025-07-08T09:00:00Z";
    if !kc::require_keycloak_or_skip("a_correction_cascades_is_recorded_and_is_reversible").await {
        return;
    }
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_correction_cascades_is_recorded_and_is_reversible",
    )
    .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    for sql in [
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name LIKE 'edit_%'",
        "DELETE FROM tool_script_activations WHERE tool_script_id IN \
         (SELECT id FROM tool_scripts WHERE name LIKE 'edit_%')",
        "DELETE FROM tool_script_versions WHERE tool_script_id IN \
         (SELECT id FROM tool_scripts WHERE name LIKE 'edit_%')",
        "DELETE FROM tool_scripts WHERE name LIKE 'edit_%'",
    ] {
        crate::common::exec(&db, sql).await;
    }
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let project_id = e2e::create_project(&app, &admin, "Edit Project", "editp", false).await;
    let site_id = e2e::create_site(&app, &admin, &project_id, "Edit Site", "edits").await;
    let pa = e2e::create_parameter(&app, &admin, "EditA", "Edit A", "ppb").await;
    let pb = e2e::create_parameter(&app, &admin, "EditB", "Edit B", "ppb").await;
    e2e::declare_site_slots(
        &db,
        &app,
        &admin,
        &site_id,
        "edit_group",
        &[(pa.as_str(), "measured"), (pb.as_str(), "output")],
    )
    .await;
    e2e::author_tool(
        &app,
        &admin,
        "edit_b",
        "tool <- function(inputs, constants, curves) list(out_b = inputs$a + 5)",
        json!({
            "label": "Edit B",
            "params": [{ "name": "a", "label": "A", "kind": "number", "required": true }],
            "event_inputs": [{ "param": "a", "parameter_code": "EditA" }],
            "outputs": [{ "key": "out_b", "label": "B", "suggested_parameter_code": "EditB" }],
        }),
        json!({ "name": "adds", "inputs": { "a": 1.0 }, "expected": { "out_b": 6.0 } }),
    )
    .await;

    kc::ensure_realm_user("river2", "river2", &["riverdata-river"]).await;
    kc::grant_project(&db, &kc::keycloak_user_id("river2").await, &project_id).await;
    let river = kc::get_keycloak_jwt("river2", "river2").await;

    let served = |parameter: String| {
        let db = db.clone();
        let site = site_id.clone();
        async move {
            use sea_orm::ConnectionTrait;
            db.query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT COALESCE(r.calibrated_value, r.raw_value) AS value FROM readings r \
                     WHERE r.site_id = '{site}' AND r.parameter_id = '{parameter}' \
                       AND r.time = '{AT}' AND r.withdrawn_at IS NULL \
                     ORDER BY r.replicate_index LIMIT 1"
                ),
            ))
            .await
            .unwrap()
            .and_then(|r| r.try_get::<Option<f64>>("", "value").ok().flatten())
        }
    };

    let (status, saved) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": site_id,
            "readings": [{ "parameter_id": pa, "value": 10.0, "time": AT }],
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "save EditA: {saved}");
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    assert_eq!(served(pb.clone()).await, Some(15.0), "the chain computed B");

    // The input is corrected through the edit primitive, previewed first as every edit is.
    let selection = json!({ "site_id": site_id, "parameter_id": pa, "from": AT, "to": AT });
    let decision = json!({ "kind": "value_correction", "value": 20.0 });
    let (status, preview) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/edits/preview",
        &json!({ "selection": selection, "decision": decision }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "preview: {preview}");
    let (status, committed) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/edits",
        &json!({
            "selection": selection,
            "decision": decision,
            "preview_id": preview["preview_id"].as_str().expect("preview id"),
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "commit: {committed}");
    assert_eq!(committed["rows_decided"], 1);

    // The cascade is what B188 found missing on the inverse: nobody asked for this recompute.
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    assert_eq!(served(pa.clone()).await, Some(20.0));
    assert_eq!(
        served(pb.clone()).await,
        Some(25.0),
        "the calculation over the corrected input ran again"
    );

    // The value's own record names the correction that moved it, and B's names the run.
    let (status, record) = crate::common::get_json_with_token(
        &app,
        &format!("/api/readings/provenance?site_id={site_id}&parameter_id={pa}&time={AT}"),
        &river,
    )
    .await;
    assert_eq!(status, 200, "provenance: {record}");
    let stream_id = record["records"][0]["origin"]["stream_id"]
        .as_str()
        .expect("the record names the stream")
        .to_string();
    let (status, decisions) = crate::common::get_json_with_token(
        &app,
        &format!("/api/readings/decisions?stream_id={stream_id}&time={AT}"),
        &river,
    )
    .await;
    assert_eq!(status, 200, "decisions: {decisions}");
    assert_eq!(
        decisions[0]["kind"], "value_correction",
        "the newest decision is the correction: {decisions}"
    );
    assert_eq!(decisions[0]["new"]["raw_value"].as_f64(), Some(20.0));
    assert_eq!(decisions[0]["old"]["raw_value"].as_f64(), Some(10.0));

    let (status, record_b) = crate::common::get_json_with_token(
        &app,
        &format!("/api/readings/provenance?site_id={site_id}&parameter_id={pb}&time={AT}"),
        &river,
    )
    .await;
    assert_eq!(status, 200, "provenance B: {record_b}");
    assert_eq!(
        record_b["records"][0]["computation"]["run_source"], "chain",
        "B's record says the chain made it: {record_b}"
    );

    // Rolling the correction back restores the input, and the cascade follows it back.
    let decision_id = committed["decision_ids"][0]
        .as_str()
        .expect("decision id")
        .to_string();
    let (status, rolled) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/readings/edits/{decision_id}/rollback"),
        &json!({}),
        &river,
    )
    .await;
    assert_eq!(status, 200, "rollback: {rolled}");
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    assert_eq!(served(pa.clone()).await, Some(10.0), "the input is back");
    assert_eq!(
        served(pb.clone()).await,
        Some(15.0),
        "and so is everything computed from it"
    );

    let (status, decisions) = crate::common::get_json_with_token(
        &app,
        &format!("/api/readings/decisions?stream_id={stream_id}&time={AT}"),
        &river,
    )
    .await;
    assert_eq!(status, 200, "decisions after rollback: {decisions}");
    assert_eq!(
        decisions[0]["kind"], "rollback",
        "the record keeps both the correction and its undo: {decisions}"
    );
    assert_eq!(
        decisions[1]["kind"], "value_correction",
        "{decisions}"
    );
    assert!(
        decisions[1]["rolled_back_by"].is_string(),
        "the correction is stamped with what undid it: {decisions}"
    );
}
