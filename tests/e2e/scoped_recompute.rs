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
    for site in [&site_id, &other_site] {
        e2e::assign_site_parameter_minimal(&app, &admin, site, &pa).await;
    }
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
}
