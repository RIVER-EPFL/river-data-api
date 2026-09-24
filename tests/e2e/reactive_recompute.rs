//! Reactive recompute: a calculation fires when a value it reads lands at a visit.
//!
//! Scenario: `react_b` reads the `ReactA` parameter at the visit and writes `ReactB`. A member
//! saves ReactA replicates by hand, with no tool run behind them, and ReactB appears with a
//! `chain` run behind it; a correction re-runs it once; the chain's own save enqueues nothing; a
//! decommissioned calculation does not fire; a flag on a replicate fires it; a portal-synced visit fires
//! it the same way (Q259); the save reports beforehand which calculation it feeds.

use serde_json::json;
use serial_test::serial;

use crate::common::e2e;
use crate::common::keycloak as kc;

const VISIT: &str = "2025-06-15T09:00:00Z";
const SYNCED_VISIT: &str = "2025-06-16T09:00:00Z";

async fn served(
    db: &sea_orm::DatabaseConnection,
    site_id: &str,
    parameter_id: &str,
) -> Option<f64> {
    river_db::routes::private::tools::flows::served_spot_value(
        db,
        site_id.parse().expect("site uuid"),
        parameter_id.parse().expect("parameter uuid"),
        VISIT.parse().expect("visit instant"),
    )
    .await
    .expect("read the served spot value")
}

#[tokio::test]
#[serial]
async fn a_value_landing_at_a_visit_runs_the_calculation_that_reads_it() {
    use sea_orm::ConnectionTrait;
    if !crate::common::profile::Service::Keycloak
        .require("a_value_landing_at_a_visit_runs_the_calculation_that_reads_it")
        .await
    {
        return;
    }
    if !crate::common::profile::Service::ToolsRunner
        .require("a_value_landing_at_a_visit_runs_the_calculation_that_reads_it")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::exec(
        &db,
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name LIKE 'react_%'",
    )
    .await;
    crate::common::exec(
        &db,
        "DELETE FROM tool_script_activations WHERE tool_script_id IN \
         (SELECT id FROM tool_scripts WHERE name LIKE 'react_%')",
    )
    .await;
    crate::common::exec(
        &db,
        "DELETE FROM tool_script_versions WHERE tool_script_id IN \
         (SELECT id FROM tool_scripts WHERE name LIKE 'react_%')",
    )
    .await;
    crate::common::exec(&db, "DELETE FROM tool_scripts WHERE name LIKE 'react_%'").await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let project_id = e2e::create_project(&app, &admin, "React Project", "reactp", false).await;
    let site_id = e2e::create_site(&app, &admin, &project_id, "React Site", "reacts").await;
    let pa = e2e::create_parameter(&app, &admin, "ReactA", "React A", "ppb").await;
    let pb = e2e::create_parameter(&app, &admin, "ReactB", "React B", "ppb").await;
    // The site declares the calculation by carrying its output slot (Q98), so the chain has
    // somewhere to land.
    e2e::declare_site_slots(
        &db,
        &app,
        &admin,
        &site_id,
        "react_group",
        &[pa.as_str(), pb.as_str()],
    )
    .await;
    e2e::author_tool(
        &app,
        &admin,
        "react_b",
        "tool <- function(inputs, constants, curves) list(out_b = inputs$a + 5)",
        json!({
            "label": "React B",
            "params": [{ "name": "a", "label": "A", "kind": "number", "required": true }],
            "event_inputs": [{ "param": "a", "parameter_code": "ReactA" }],
            "outputs": [{ "key": "out_b", "label": "B", "suggested_parameter_code": "ReactB" }],
        }),
        json!({ "name": "adds", "inputs": { "a": 1.0 }, "expected": { "out_b": 6.0 } }),
    )
    .await;

    kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    kc::grant_project(&db, &kc::keycloak_user_id("river1").await, &project_id).await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;

    let save_a = |values: Vec<f64>, at: &'static str, replace: bool, dry_run: bool| {
        let app = app.clone();
        let river = river.clone();
        let site_id = site_id.clone();
        let pa = pa.clone();
        async move {
            let readings: Vec<serde_json::Value> = values
                .iter()
                .map(|v| json!({ "parameter_id": pa, "value": v, "time": at }))
                .collect();
            let mut body = json!({ "site_id": site_id, "readings": readings, "dry_run": dry_run });
            if replace {
                body["mode"] = json!("replace");
            }
            let (status, resp) = crate::common::post_checked_grab_parse(&app, &body, &river).await;
            assert_eq!(status, 200, "save ReactA: {resp}");
            resp
        }
    };
    let recompute_jobs = || {
        let db = db.clone();
        async move {
            e2e::count(
                &db,
                "SELECT COUNT(*)::bigint FROM reprocessing_jobs WHERE trigger_type = 'event_recompute'",
            )
            .await
        }
    };
    let chain_runs = || {
        let db = db.clone();
        async move {
            e2e::count(
                &db,
                "SELECT COUNT(*)::bigint FROM tool_runs WHERE tool_name = 'react_b' AND source = 'chain'",
            )
            .await
        }
    };

    // Before the write, the save says which calculation the value feeds and what it rewrites.
    let preview = save_a(vec![10.0, 20.0], VISIT, false, true).await;
    let fed = preview["calculations"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(fed.len(), 1, "one calculation reads ReactA: {preview}");
    assert_eq!(fed[0]["tool"], "react_b");
    assert_eq!(fed[0]["outputs"][0]["parameter_code"], "ReactB");
    assert_eq!(recompute_jobs().await, 0, "a dry run enqueues nothing");

    // The hand-entered replicates land, and the calculation that reads their mean fires.
    save_a(vec![10.0, 20.0], VISIT, false, false).await;
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    assert_eq!(chain_runs().await, 1, "react_b ran once from the save");
    assert_eq!(
        served(&db, &site_id, &pb).await,
        Some(20.0),
        "mean(10, 20) + 5"
    );
    assert_eq!(
        recompute_jobs().await,
        1,
        "the chain's own save enqueued nothing"
    );
    assert_eq!(
        e2e::count(
            &db,
            "SELECT COUNT(*)::bigint FROM reprocessing_jobs \
             WHERE trigger_type = 'event_recompute' AND dedupe_key IS NOT NULL",
        )
        .await,
        0,
        "the claim released the dedupe key"
    );

    // A correction re-runs it exactly once and the output moves.
    save_a(vec![10.0, 30.0], VISIT, true, false).await;
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    assert_eq!(chain_runs().await, 2);
    assert_eq!(
        served(&db, &site_id, &pb).await,
        Some(25.0),
        "mean(10, 30) + 5"
    );
    assert_eq!(recompute_jobs().await, 2);

    // Decommissioned, the calculation is not part of the set a save feeds: nothing is enqueued.
    e2e::set_live(&app, &admin, "react_b", false).await;
    let preview = save_a(vec![10.0, 40.0], VISIT, true, true).await;
    assert!(
        preview["calculations"].as_array().unwrap().is_empty(),
        "a decommissioned calculation is not reported: {preview}"
    );
    save_a(vec![10.0, 40.0], VISIT, true, false).await;
    assert_eq!(
        recompute_jobs().await,
        2,
        "a decommissioned calculation enqueues nothing"
    );
    assert_eq!(chain_runs().await, 2);
    assert_eq!(
        served(&db, &site_id, &pb).await,
        Some(25.0),
        "the output stands"
    );

    // Recommissioned, a flag on one replicate changes the served mean and fires it again.
    e2e::set_live(&app, &admin, "react_b", true).await;
    let (status, flagged) = crate::common::patch_json_with_token(
        &app,
        "/api/readings/flag",
        &json!({
            "readings": [{ "site_id": site_id, "parameter_id": pa, "time": VISIT,
                            "replicate_index": 1, "measurement_type": "spot" }],
            "reason": "vial cracked",
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{flagged}");
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    assert_eq!(recompute_jobs().await, 3, "the flag enqueued a recompute");
    assert_eq!(chain_runs().await, 3);
    assert_eq!(
        served(&db, &site_id, &pb).await,
        Some(15.0),
        "the one live replicate + 5"
    );

    // The visits grid reports the event as current once the run has landed.
    let (status, visits) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{site_id}/visits"), &river)
            .await;
    assert_eq!(status, 200, "{visits}");
    let row = visits["visits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| {
            v["collected_at"]
                .as_str()
                .unwrap()
                .starts_with("2025-06-15")
        })
        .expect("the visit is listed");
    assert_eq!(row["recompute"], "current", "{row}");

    // A portal-synced visit fires it too, and the output the portal does not compute lands (Q259).
    db.execute_unprepared(&format!(
        "INSERT INTO collection_events (site_id, collected_at, source) \
         VALUES ('{site_id}', '{SYNCED_VISIT}', 'portal_sync')"
    ))
    .await
    .unwrap();
    save_a(vec![10.0, 20.0], SYNCED_VISIT, false, false).await;
    assert_eq!(
        recompute_jobs().await,
        4,
        "a portal_sync visit enqueues its recompute"
    );
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    assert_eq!(
        river_db::routes::private::tools::flows::served_spot_value(
            &db,
            site_id.parse().expect("site uuid"),
            pb.parse().expect("parameter uuid"),
            SYNCED_VISIT.parse().expect("visit instant"),
        )
        .await
        .expect("read the served spot value"),
        Some(20.0),
        "mean(10, 20) + 5 at the synced visit"
    );
}

/// Expected behaviour: an admin detaches an output slot at a visit and edits its value; an edit to
/// an input at the visit leaves the override standing; the admin returns the slot and the visit
/// recomputes from the inputs as they now stand.
#[tokio::test]
#[serial]
async fn detach_edit_and_return_on_an_output_slot() {
    use sea_orm::ConnectionTrait;
    if !crate::common::profile::Service::Keycloak
        .require("detach_edit_and_return_on_an_output_slot")
        .await
    {
        return;
    }
    if !crate::common::profile::Service::ToolsRunner
        .require("detach_edit_and_return_on_an_output_slot")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::exec(
        &db,
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name LIKE 'react_%'",
    )
    .await;
    crate::common::exec(
        &db,
        "DELETE FROM tool_script_activations WHERE tool_script_id IN \
         (SELECT id FROM tool_scripts WHERE name LIKE 'react_%')",
    )
    .await;
    crate::common::exec(
        &db,
        "DELETE FROM tool_script_versions WHERE tool_script_id IN \
         (SELECT id FROM tool_scripts WHERE name LIKE 'react_%')",
    )
    .await;
    crate::common::exec(&db, "DELETE FROM tool_scripts WHERE name LIKE 'react_%'").await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let project_id = e2e::create_project(&app, &admin, "Own Project", "ownp", false).await;
    let site_id = e2e::create_site(&app, &admin, &project_id, "Own Site", "owns").await;
    let pa = e2e::create_parameter(&app, &admin, "ReactA", "React A", "ppb").await;
    let pb = e2e::create_parameter(&app, &admin, "ReactB", "React B", "ppb").await;
    // The site declares the calculation by carrying its output slot (Q98), so the chain has
    // somewhere to land.
    e2e::declare_site_slots(
        &db,
        &app,
        &admin,
        &site_id,
        "own_react_group",
        &[pa.as_str(), pb.as_str()],
    )
    .await;
    e2e::author_tool(
        &app,
        &admin,
        "react_b",
        "tool <- function(inputs, constants, curves) list(out_b = inputs$a + 5)",
        json!({
            "label": "React B",
            "params": [{ "name": "a", "label": "A", "kind": "number", "required": true }],
            "event_inputs": [{ "param": "a", "parameter_code": "ReactA" }],
            "outputs": [{ "key": "out_b", "label": "B", "suggested_parameter_code": "ReactB" }],
        }),
        json!({ "name": "adds", "inputs": { "a": 1.0 }, "expected": { "out_b": 6.0 } }),
    )
    .await;

    kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    kc::grant_project(&db, &kc::keycloak_user_id("river1").await, &project_id).await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;

    // Seed A = 10 at the visit: the save fires B = 15.
    let (status, _) = crate::common::post_checked_grab(
        &app,
        &json!({ "site_id": site_id, "readings": [{ "parameter_id": pa, "value": 10.0, "time": VISIT }] }),
        &river,
    )
    .await;
    assert_eq!(status, 200);
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    assert_eq!(served(&db, &site_id, &pb).await, Some(15.0));

    // The admin detaches B at this visit: it is now manual.
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/detach",
        &json!({ "site_id": site_id, "parameter_id": pb, "time": VISIT, "reason": "override" }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["owner"], "manual");

    // The admin corrects B to 99: a value_correction decision, the tool's value is gone.
    let (status, _) = crate::common::post_checked_grab(
        &app,
        &json!({
            "site_id": site_id,
            "mode": "replace",
            "readings": [{ "parameter_id": pb, "value": 99.0, "time": VISIT }],
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(served(&db, &site_id, &pb).await, Some(99.0));

    // A correction to the input at the visit leaves the override standing.
    let (status, _) = crate::common::post_checked_grab(
        &app,
        &json!({
            "site_id": site_id,
            "mode": "replace",
            "readings": [{ "parameter_id": pa, "value": 20.0, "time": VISIT }],
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200);
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    assert_eq!(
        served(&db, &site_id, &pb).await,
        Some(99.0),
        "the override stands"
    );

    // The return recomputes the visit: the tool's value follows the corrected input.
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/return",
        &json!({ "site_id": site_id, "parameter_id": pb, "time": VISIT }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["owner"], "tool");
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    assert_eq!(
        served(&db, &site_id, &pb).await,
        Some(25.0),
        "returned and recomputed: 20 + 5"
    );

    // After the return the tool owns the slot: returning again is refused, detaching is valid.
    let (status, _) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/return",
        &json!({ "site_id": site_id, "parameter_id": pb, "time": VISIT }),
        &admin,
    )
    .await;
    assert_eq!(status, 409, "returning a tool-owned slot is refused");
    let (status, _) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/detach",
        &json!({ "site_id": site_id, "parameter_id": pb, "time": VISIT }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "detaching a tool-owned slot is valid");
    // Now detaching again is refused.
    let (status, _) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/detach",
        &json!({ "site_id": site_id, "parameter_id": pb, "time": VISIT }),
        &admin,
    )
    .await;
    assert_eq!(status, 409, "detaching an already-detached slot is refused");

    // The decision history shows the whole ownership sequence.
    let stream_id = db
        .query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT stream_id::text FROM readings \
                 WHERE site_id = '{site_id}' AND parameter_id = '{pb}' AND time = '{VISIT}' LIMIT 1"
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<String>("", "stream_id")
        .unwrap();
    let (status, history) = crate::common::get_json_with_token(
        &app,
        &format!("/api/readings/decisions?stream_id={stream_id}&time={VISIT}"),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{history}");
    let kinds: Vec<&str> = history
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.contains(&"detach") && kinds.contains(&"return") && kinds.contains(&"chain"),
        "the ownership history: {kinds:?}"
    );
}
