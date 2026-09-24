//! S2, collection event with chained tools (story catalog: ../archived-documentation/PLAN.md).
//!
//! Scenario: a member stages a visit, runs one tool and saves it; a second tool's `event_inputs`
//! resolve from the first tool's saved output at the same (site, collected_at) and it runs by
//! itself (ADR 0007). A third tool, recommissioned after the visit was entered, is what the
//! missing/stale audit reports; the chain executor recomputes the event on demand and fills it,
//! and the trigger statistics are correct throughout.

use serde_json::json;
use serial_test::serial;

use crate::common::e2e;
use crate::common::keycloak as kc;

/// The id of the catalog parameter a calculation minted for `code`.
async fn minted_output(db: &sea_orm::DatabaseConnection, code: &str) -> String {
    use sea_orm::ConnectionTrait;
    db.query_one_raw(sea_orm::Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT id FROM parameters WHERE lower(code) = lower('{code}')"),
    ))
    .await
    .expect("the catalog reads")
    .expect("the calculation minted its output")
    .try_get::<uuid::Uuid>("", "id")
    .expect("id")
    .to_string()
}

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

/// The site declares the slots the chain writes: a grab save refuses a parameter the site does not
/// carry (Q98), so every story applies the group its three parameters belong to before saving.
async fn declare_chain_slots(
    db: &sea_orm::DatabaseConnection,
    app: &axum::Router,
    admin: &str,
    site_id: &str,
    pa: &str,
    pb: &str,
    pc: &str,
) -> String {
    let group_id = uuid::Uuid::new_v4().to_string();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO parameter_groups (id, code, label, ordinal) \
             VALUES ('{group_id}', 'chain_group', 'Chain group', 1)"
        ),
    )
    .await;
    for (parameter, ordinal) in [(pa, 1), (pb, 2), (pc, 3)] {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal) \
                 VALUES (gen_random_uuid(), '{group_id}', '{parameter}', {ordinal})"
            ),
        )
        .await;
    }
    let (status, applied) = crate::common::post_json_with_token(
        app,
        &format!("/api/sites/{site_id}/parameter_groups"),
        &json!({ "group_id": group_id }),
        admin,
    )
    .await;
    assert_eq!(status, 200, "apply the group: {applied}");
    group_id
}

/// A member added to a group the site already carries: the group is applied again, which adds only
/// what is missing.
async fn add_slot(
    db: &sea_orm::DatabaseConnection,
    app: &axum::Router,
    admin: &str,
    site_id: &str,
    group_id: &str,
    parameter: &str,
    ordinal: i32,
) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal) \
             VALUES (gen_random_uuid(), '{group_id}', '{parameter}', {ordinal})"
        ),
    )
    .await;
    let (status, applied) = crate::common::post_json_with_token(
        app,
        &format!("/api/sites/{site_id}/parameter_groups"),
        &json!({ "group_id": group_id }),
        admin,
    )
    .await;
    assert_eq!(status, 200, "apply the group again: {applied}");
}

/// Expected behaviour: saving A's output fires B by itself (ADR 0007); C, decommissioned while the
/// visit was entered, is what the audit reports missing once it is recommissioned, and the
/// on-demand executor fills it in dependency order with trigger statistics and a chain-run blob.
#[tokio::test]
#[serial]
async fn two_tools_share_an_event_and_the_audit_and_executor_close_the_gap() {
    if !crate::common::profile::Service::Keycloak
        .require("two_tools_share_an_event_and_the_audit_and_executor_close_the_gap")
        .await
    {
        return;
    }
    if !crate::common::profile::Service::ToolsRunner
        .require("two_tools_share_an_event_and_the_audit_and_executor_close_the_gap")
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
    e2e::set_live(&app, &admin, "chain_c", false).await;
    let group_id = declare_chain_slots(&db, &app, &admin, &site_id, &pa, &pb, &pc).await;

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
    let (status, saved) = crate::common::post_checked_grab(
        &app,
        &json!({
            "site_id": site_id,
            "tool_run_id": a["run_id"],
            "readings": [{ "parameter_id": pa, "value": 42.0, "time": EVENT_TIME, "output": "out_a" }],
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "save A: {saved}");

    // A's output landing is what runs B: its input resolves from the saved value at the shared
    // event, and nobody presses anything.
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    let reactive = {
        use sea_orm::ConnectionTrait;
        db.query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COALESCE(r.calibrated_value, r.raw_value) AS mean, \
                        r.provenance ->> 'tool' AS tool \
                 FROM readings r \
                 WHERE r.site_id = '{site_id}' AND r.parameter_id = '{pb}' \
                   AND r.withdrawn_at IS NULL \
                 ORDER BY r.replicate_index LIMIT 1"
            ),
        ))
        .await
        .unwrap()
        .expect("the save fired B")
    };
    assert_eq!(
        reactive.try_get::<Option<f64>>("", "mean").unwrap(),
        Some(47.0)
    );
    assert_eq!(reactive.try_get::<String>("", "tool").unwrap(), "chain_b");

    // Run interactively, B records the same resolution.
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
    let (status, saved) = crate::common::post_checked_grab(
        &app,
        &json!({
            "site_id": site_id,
            "tool_run_id": b["run_id"],
            "readings": [{ "parameter_id": b["event_inputs"][0]["parameter_id"], "value": 0.0,
                            "time": EVENT_TIME, "output": "out_b" }],
        }),
        &river,
    )
    .await;
    // A wrong parameter/value pairing is refused.
    assert_eq!(
        status, 400,
        "a value the run did not produce is refused: {saved}"
    );

    // C is recommissioned after the visit was entered: the audit reports its absent output, since
    // its input (ChainPB) exists.
    e2e::set_live(&app, &admin, "chain_c", true).await;
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

    let findings =
        serde_json::Value::Array(e2e::pending_event_findings(&app, &admin, &site_id).await);
    let missing: Vec<&serde_json::Value> = findings
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["kind"] == "missing_output" && f["tool"] == "chain_c")
        .collect();
    assert_eq!(
        missing.len(),
        1,
        "chain_c's absent output is reported: {findings}"
    );
    assert_eq!(
        missing[0]["parameter_code"], "ChainPC",
        "the output slot is {pc}"
    );

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
        db.query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COALESCE(s.mean, COALESCE(r.calibrated_value, r.raw_value)) AS mean, \
                        COALESCE(s.n, 1) AS n, r.provenance ->> 'tool' AS tool, \
                        r.provenance ->> 'source' AS source \
                 FROM readings r LEFT JOIN samples s ON s.id = r.sample_id \
                 WHERE r.site_id = '{site_id}' AND r.parameter_id = '{pc}' \
                 ORDER BY r.replicate_index LIMIT 1"
            ),
        ))
        .await
        .unwrap()
        .expect("the executor saved the ChainPC value")
    };
    assert_eq!(row.try_get::<Option<f64>>("", "mean").unwrap(), Some(470.0));
    assert_eq!(row.try_get::<i32>("", "n").unwrap(), 1);
    assert_eq!(row.try_get::<String>("", "tool").unwrap(), "chain_c");
    assert_eq!(row.try_get::<String>("", "source").unwrap(), "tool_run");

    // The readings carry the event, and a fresh audit supersedes the finding.
    let attached = {
        use sea_orm::ConnectionTrait;
        db.query_one_raw(sea_orm::Statement::from_string(
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
    let findings =
        serde_json::Value::Array(e2e::pending_event_findings(&app, &admin, &site_id).await);
    assert!(
        findings.as_array().unwrap().is_empty(),
        "the filled event has no open findings: {findings}"
    );

    // The two engines in one order. `chain_f` is a formula calculation reading ChainPA and writing
    // ChainPF; `chain_g` is a script reading ChainPF. A formula calculation reaches the ordering
    // only through the manifest its formulas synthesise, so ChainPG holding the right number is
    // the assertion that the synthesised manifest produced the edge: it is reachable in one pass
    // only if F was ordered before G.
    let pg = e2e::create_parameter(&app, &admin, "ChainPG", "Chain PG", "ppb").await;
    // A parameter belongs to one group, so the formula's output joins the group the chain already
    // holds rather than a second one naming ChainPA again. ChainPF is minted by the formula
    // itself (Q191), so its slot is declared once the formula below has created it.
    add_slot(&db, &app, &admin, &site_id, &group_id, &pg, 5).await;
    // A formula calculation is authored as a `tool_scripts` row bound to the group, then a formula
    // in it; the version is minted from the formula set rather than posted as a body.
    crate::common::exec(
        &db,
        "INSERT INTO tool_scripts (name, label, engine, created_by) \
             VALUES ('chain_f', 'Chain F', 'formula', 'test')",
    )
    .await;
    let (status, scripts) =
        crate::common::get_json_with_token(&app, "/api/tool_scripts", &admin).await;
    assert_eq!(status, 200, "{scripts}");
    let chain_f_id = scripts
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "chain_f")
        .map(|s| s["id"].as_str().unwrap().to_string())
        .expect("chain_f is listed");
    let (status, formula) = crate::common::save_formula_set(
        &app,
        &admin,
        &chain_f_id,
        json!([{
            "code": "ChainPF", "name": "ChainPF", "units": "ppb",
            "formula": "ChainPA * 2", "ordinal": 1,
        }]),
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "the formula ({status}): {formula}"
    );
    let pf = minted_output(&db, "ChainPF").await;
    add_slot(&db, &app, &admin, &site_id, &group_id, &pf, 4).await;

    e2e::author_tool(
        &app,
        &admin,
        "chain_g",
        "tool <- function(inputs, constants, curves) list(out_g = inputs$pf + 1)",
        json!({
            "label": "Chain G",
            "params": [{ "name": "pf", "label": "PF", "kind": "number", "required": true }],
            "event_inputs": [{ "param": "pf", "parameter_code": "ChainPF" }],
            "outputs": [{ "key": "out_g", "label": "PG", "suggested_parameter_code": "ChainPG" }],
        }),
        json!({ "name": "adds one", "inputs": { "pf": 1.0 }, "expected": { "out_g": 2.0 } }),
    )
    .await;

    let (status, recompute) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/collection_events/{event_id}/recompute"),
        &json!({}),
        &river,
    )
    .await;
    assert_eq!(status, 200, "recompute with both engines: {recompute}");
    let job_id = recompute["job_id"].as_str().expect("job id").to_string();
    assert_eq!(e2e::poll_job(&app, &admin, &job_id, 60).await, "completed");

    let stored = |parameter: String| {
        let db = db.clone();
        let site_id = site_id.clone();
        async move {
            use sea_orm::ConnectionTrait;
            db.query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT COALESCE(r.calibrated_value, r.raw_value) AS value, \
                            r.provenance ->> 'tool' AS tool \
                     FROM readings r \
                     WHERE r.site_id = '{site_id}' AND r.parameter_id = '{parameter}' \
                       AND r.withdrawn_at IS NULL ORDER BY r.replicate_index LIMIT 1"
                ),
            ))
            .await
            .unwrap()
            .map(|r| {
                (
                    r.try_get::<Option<f64>>("", "value").unwrap(),
                    r.try_get::<String>("", "tool").unwrap(),
                )
            })
        }
    };
    // ChainPF = 42 * 2, by the formula engine.
    assert_eq!(
        stored(pf.clone()).await,
        Some((Some(84.0), "chain_f".to_string())),
        "the formula calculation stored its output"
    );
    // ChainPG = 84 + 1, by the script engine, from a value the formula produced in the same pass.
    assert_eq!(
        stored(pg.clone()).await,
        Some((Some(85.0), "chain_g".to_string())),
        "the script ran after the formula and read what it wrote"
    );
}

/// Expected behaviour: an upstream correction landing while the downstream calculations are
/// decommissioned leaves their outputs demonstrably stale (the audit recomputes each saved output
/// under its pinned version with the event's current values and reports the disagreement), and
/// the chain executor converges the event, after which the audit finds nothing. (Live, the
/// correction itself would have re-run them: ADR 0007.)
#[tokio::test]
#[serial]
async fn an_upstream_correction_surfaces_as_stale_and_recompute_converges() {
    use sea_orm::ConnectionTrait;
    if !crate::common::profile::Service::Keycloak
        .require("an_upstream_correction_surfaces_as_stale_and_recompute_converges")
        .await
    {
        return;
    }
    if !crate::common::profile::Service::ToolsRunner
        .require("an_upstream_correction_surfaces_as_stale_and_recompute_converges")
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
    declare_chain_slots(&db, &app, &admin, &site_id, &pa, &pb, &pc).await;

    kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    kc::grant_project(&db, &kc::keycloak_user_id("river1").await, &project_id).await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;

    let save =
        |run: serde_json::Value, param: String, value: f64, output: &'static str, replace: bool| {
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
                let (status, resp) = crate::common::post_checked_grab(&app, &body, &river).await;
                assert_eq!(status, 200, "save {output}: {resp}");
            }
        };

    // The initial chain: A(21) -> 42 saved, and the save runs B (47) and C (470) by itself.
    let (status, a) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/chain_a/calculate",
        &json!({ "a": 21.0, "site_id": site_id, "collected_at": EVENT_TIME }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{a}");
    save(a, pa.clone(), 42.0, "out_a", false).await;
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    for (param, expected) in [(&pb, 47.0), (&pc, 470.0)] {
        let mean = db
            .query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT COALESCE(calibrated_value, raw_value) AS mean FROM readings \
                     WHERE site_id = '{site_id}' AND parameter_id = '{param}' \
                       AND withdrawn_at IS NULL AND is_flagged IS NOT TRUE \
                     ORDER BY replicate_index LIMIT 1"
                ),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<Option<f64>>("", "mean")
            .unwrap();
        assert_eq!(mean, Some(expected), "the save fired the chain for {param}");
    }

    // The upstream correction lands while B and C are decommissioned: A's input was mistyped and
    // the corrected run replaces PA with 50, and nothing downstream moves.
    e2e::set_live(&app, &admin, "chain_b", false).await;
    e2e::set_live(&app, &admin, "chain_c", false).await;
    let (status, a2) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/chain_a/calculate",
        &json!({ "a": 25.0, "site_id": site_id, "collected_at": EVENT_TIME }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{a2}");
    save(a2, pa.clone(), 50.0, "out_a", true).await;
    assert_eq!(
        crate::common::e2e::count(
            &db,
            "SELECT COUNT(*)::bigint FROM tool_runs WHERE tool_name = 'chain_b' AND source = 'chain'",
        )
        .await,
        1,
        "a decommissioned calculation does not fire"
    );
    e2e::set_live(&app, &admin, "chain_b", true).await;
    e2e::set_live(&app, &admin, "chain_c", true).await;

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
    let findings =
        serde_json::Value::Array(e2e::pending_event_findings(&app, &admin, &site_id).await);
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
        .query_one_raw(sea_orm::Statement::from_string(
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
            .query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT COALESCE(s.mean, COALESCE(r.calibrated_value, r.raw_value)) AS mean \
                     FROM readings r LEFT JOIN samples s ON s.id = r.sample_id \
                     WHERE r.site_id = '{site_id}' AND r.parameter_id = '{param}' \
                     ORDER BY r.replicate_index LIMIT 1"
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
    let findings =
        serde_json::Value::Array(e2e::pending_event_findings(&app, &admin, &site_id).await);
    assert!(
        findings.as_array().unwrap().is_empty(),
        "the converged event has no open findings: {findings}"
    );
}

/// Expected behaviour: a recompute of a visit whose inputs, constants, curves and script versions
/// have not moved since the last run mints no `tool_runs` row and rewrites no output, and a
/// changed upstream input runs each tool downstream of it exactly once.
#[tokio::test]
#[serial]
async fn an_unchanged_visit_recomputes_nothing_and_a_changed_input_reruns_once() {
    use sea_orm::ConnectionTrait;
    if !crate::common::profile::Service::Keycloak
        .require("an_unchanged_visit_recomputes_nothing_and_a_changed_input_reruns_once")
        .await
    {
        return;
    }
    if !crate::common::profile::Service::ToolsRunner
        .require("an_unchanged_visit_recomputes_nothing_and_a_changed_input_reruns_once")
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

    let project_id = e2e::create_project(&app, &admin, "Idem Project", "idemp", false).await;
    let site_id = e2e::create_site(&app, &admin, &project_id, "Idem Site", "idems").await;
    let pa = e2e::create_parameter(&app, &admin, "ChainPA", "Chain PA", "ppb").await;
    let pb = e2e::create_parameter(&app, &admin, "ChainPB", "Chain PB", "ppb").await;
    let pc = e2e::create_parameter(&app, &admin, "ChainPC", "Chain PC", "ppb").await;
    author_chain(&app, &admin).await;
    declare_chain_slots(&db, &app, &admin, &site_id, &pa, &pb, &pc).await;

    kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    kc::grant_project(&db, &kc::keycloak_user_id("river1").await, &project_id).await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;

    let calculate_and_save_a = |a: f64, replace: bool| {
        let app = app.clone();
        let river = river.clone();
        let site_id = site_id.clone();
        let pa = pa.clone();
        async move {
            let (status, run) = crate::common::post_json_parse_with_token(
                &app,
                "/api/tools/chain_a/calculate",
                &json!({ "a": a, "site_id": site_id, "collected_at": EVENT_TIME }),
                &river,
            )
            .await;
            assert_eq!(status, 200, "{run}");
            let mut body = json!({
                "site_id": site_id,
                "tool_run_id": run["run_id"],
                "readings": [{ "parameter_id": pa, "value": a * 2.0,
                                "time": EVENT_TIME, "output": "out_a" }],
            });
            if replace {
                body["mode"] = json!("replace");
            }
            let (status, resp) = crate::common::post_checked_grab(&app, &body, &river).await;
            assert_eq!(status, 200, "save out_a: {resp}");
        }
    };
    let recompute = |event_id: String| {
        let app = app.clone();
        let river = river.clone();
        let admin = admin.clone();
        async move {
            let (status, resp) = crate::common::post_json_parse_with_token(
                &app,
                &format!("/api/collection_events/{event_id}/recompute"),
                &json!({}),
                &river,
            )
            .await;
            assert_eq!(status, 200, "{resp}");
            let job_id = resp["job_id"].as_str().expect("job id").to_string();
            assert_eq!(e2e::poll_job(&app, &admin, &job_id, 60).await, "completed");
            job_id
        }
    };
    let runs = |tool: &'static str| {
        let db = db.clone();
        async move {
            e2e::count(
                &db,
                &format!(
                    "SELECT COUNT(*)::bigint FROM tool_runs \
                     WHERE tool_name = '{tool}' AND source = 'chain'"
                ),
            )
            .await
        }
    };
    let saved_at = |param: String| {
        let db = db.clone();
        let site_id = site_id.clone();
        async move {
            db.query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT provenance ->> 'saved_at' AS saved_at FROM readings \
                     WHERE site_id = '{site_id}' AND parameter_id = '{param}' \
                       AND provenance IS NOT NULL ORDER BY replicate_index LIMIT 1"
                ),
            ))
            .await
            .unwrap()
            .expect("the output has a sample")
            .try_get::<String>("", "saved_at")
            .unwrap()
        }
    };
    let job_count = |job_id: String, key: &'static str| {
        let db = db.clone();
        async move {
            e2e::count(
                &db,
                &format!(
                    "SELECT COALESCE((detail -> 'counts' ->> '{key}')::bigint, 0) \
                     FROM reprocessing_jobs WHERE id = '{job_id}'"
                ),
            )
            .await
        }
    };

    // The save fires B and C once each; A's stored inputs are the interactive run's.
    calculate_and_save_a(21.0, false).await;
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    assert_eq!(runs("chain_a").await, 0);
    assert_eq!(runs("chain_b").await, 1);
    assert_eq!(runs("chain_c").await, 1);
    let event_id = db
        .query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT id::text AS id FROM collection_events \
                 WHERE site_id = '{site_id}' AND collected_at = '{EVENT_TIME}'"
            ),
        ))
        .await
        .unwrap()
        .expect("the save attached a collection event")
        .try_get::<String>("", "id")
        .unwrap();
    let b_saved_at = saved_at(pb.clone()).await;
    let c_saved_at = saved_at(pc.clone()).await;

    // Nothing moved: a recompute mints no run and rewrites no provenance, and says why.
    let unchanged = recompute(event_id.clone()).await;
    assert_eq!(runs("chain_a").await, 0, "unchanged A re-ran");
    assert_eq!(runs("chain_b").await, 1, "unchanged B re-ran");
    assert_eq!(runs("chain_c").await, 1, "unchanged C re-ran");
    assert_eq!(
        saved_at(pb.clone()).await,
        b_saved_at,
        "B's provenance was rewritten"
    );
    assert_eq!(
        saved_at(pc.clone()).await,
        c_saved_at,
        "C's provenance was rewritten"
    );
    assert_eq!(job_count(unchanged.clone(), "tools_run").await, 0);
    assert_eq!(job_count(unchanged, "tools_unchanged").await, 3);

    // A corrected upstream input: B and C each run exactly once more, A not at all, and a
    // recompute after that is again unchanged.
    calculate_and_save_a(25.0, true).await;
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    assert_eq!(runs("chain_a").await, 0);
    assert_eq!(runs("chain_b").await, 2, "B runs once for the correction");
    assert_eq!(runs("chain_c").await, 2, "C runs once for the correction");
    assert_ne!(saved_at(pb.clone()).await, b_saved_at);
    assert_ne!(saved_at(pc.clone()).await, c_saved_at);
    for (param, expected) in [(&pb, 55.0), (&pc, 550.0)] {
        let mean = db
            .query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT COALESCE(calibrated_value, raw_value) AS mean FROM readings \
                     WHERE site_id = '{site_id}' AND parameter_id = '{param}' \
                       AND withdrawn_at IS NULL AND is_flagged IS NOT TRUE \
                     ORDER BY replicate_index LIMIT 1"
                ),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<Option<f64>>("", "mean")
            .unwrap();
        assert_eq!(mean, Some(expected), "converged value for {param}");
    }
    let again = recompute(event_id).await;
    assert_eq!(runs("chain_b").await, 2);
    assert_eq!(runs("chain_c").await, 2);
    assert_eq!(job_count(again.clone(), "tools_run").await, 0);
    assert_eq!(job_count(again, "tools_unchanged").await, 3);
}

/// The same three tools, with B raising on the value A produces at this visit. The golden case
/// stays inside the guard so the version still validates: the failure is in the data, not the
/// script.
async fn author_chain_with_failing_b(app: &axum::Router, admin: &str) {
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
        "tool <- function(inputs, constants, curves) {\n\
         if (inputs$pa > 100) stop(\"division by zero\")\n\
         list(out_b = inputs$pa + 5)\n\
         }",
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

/// Expected behaviour: a script error partway through a cascade is a skip, not a failure. The
/// step that raised writes nothing, every step downstream of it skips for want of an input, and
/// the run completes with both skips and their reasons on the job. Each absent output is also a
/// finding in the review queue carrying the reason, so the visit reads `stale` and the account
/// outlives the job row.
#[tokio::test]
#[serial]
async fn a_script_error_midway_skips_its_step_and_everything_downstream() {
    use sea_orm::ConnectionTrait;
    if !crate::common::profile::Service::Keycloak
        .require("a_script_error_midway_skips_its_step_and_everything_downstream")
        .await
    {
        return;
    }
    if !crate::common::profile::Service::ToolsRunner
        .require("a_script_error_midway_skips_its_step_and_everything_downstream")
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

    let project_id = e2e::create_project(&app, &admin, "Raise Project", "raisep", false).await;
    let site_id = e2e::create_site(&app, &admin, &project_id, "Raise Site", "raises").await;
    let pa = e2e::create_parameter(&app, &admin, "ChainPA", "Chain PA", "ppb").await;
    let pb = e2e::create_parameter(&app, &admin, "ChainPB", "Chain PB", "ppb").await;
    let pc = e2e::create_parameter(&app, &admin, "ChainPC", "Chain PC", "ppb").await;
    author_chain_with_failing_b(&app, &admin).await;

    declare_chain_slots(&db, &app, &admin, &site_id, &pa, &pb, &pc).await;

    kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    kc::grant_project(&db, &kc::keycloak_user_id("river1").await, &project_id).await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;

    let (status, event) = crate::common::post_json_parse_with_token(
        &app,
        "/api/collection_events",
        &json!({ "site_id": site_id, "collected_at": EVENT_TIME }),
        &river,
    )
    .await;
    assert!((200..300).contains(&status), "stage ({status}): {event}");
    let event_id = e2e::id_of(&event);

    // A produces 120, which is what B raises on.
    let (status, a) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/chain_a/calculate",
        &json!({ "a": 60.0, "site_id": site_id, "collected_at": EVENT_TIME }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "calculate A ({status}): {a}");
    let (status, saved) = crate::common::post_checked_grab(
        &app,
        &json!({
            "site_id": site_id,
            "tool_run_id": a["run_id"],
            "readings": [{ "parameter_id": pa, "value": 120.0, "time": EVENT_TIME, "output": "out_a" }],
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "save A: {saved}");
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/collection_events/{event_id}/recompute"),
        &json!({}),
        &river,
    )
    .await;
    assert_eq!(status, 200, "recompute enqueue: {resp}");
    let job_id = resp["job_id"].as_str().expect("job id").to_string();
    assert_eq!(
        e2e::poll_job(&app, &admin, &job_id, 60).await,
        "completed",
        "a raising step does not fail the run"
    );

    // Stage 1 is written; stage 2 and stage 3 are not.
    let served = |parameter: String| {
        let db = db.clone();
        let site_id = site_id.clone();
        async move {
            db.query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT COALESCE(calibrated_value, raw_value) AS value FROM readings \
                     WHERE site_id = '{site_id}' AND parameter_id = '{parameter}' \
                       AND withdrawn_at IS NULL ORDER BY replicate_index LIMIT 1"
                ),
            ))
            .await
            .unwrap()
            .map(|r| r.try_get::<Option<f64>>("", "value").unwrap())
        }
    };
    assert_eq!(served(pa).await, Some(Some(120.0)));
    assert_eq!(served(pb).await, None, "the raising step wrote a value");
    assert_eq!(served(pc).await, None, "a step downstream of the raise ran");

    // What the job reports: both steps skipped, with the script's message on the one that raised.
    let (status, job) = crate::common::get_json_with_token(
        &app,
        &format!("/api/reprocessing_jobs/{job_id}"),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{job}");
    assert_eq!(job["detail"]["counts"]["tools_skipped"], 2, "{job}");
    assert_eq!(job["detail"]["counts"]["readings_written"], 0, "{job}");
    let skipped = job["detail"]["scope"]["skipped"]
        .as_array()
        .expect("the skips are reported");
    let b_reason = skipped
        .iter()
        .find(|s| s["tool"] == "chain_b")
        .and_then(|s| s["reason"].as_str())
        .expect("chain_b is reported skipped");
    assert!(
        b_reason.starts_with("script error: "),
        "the script's own message is carried: {b_reason}"
    );
    assert!(
        b_reason.contains("division by zero"),
        "the script's own message is carried: {b_reason}"
    );
    assert!(
        skipped.iter().any(|s| s["tool"] == "chain_c"),
        "chain_c skips for want of B's output: {job}"
    );

    assert_eq!(job["detail"]["counts"]["findings_raised"], 2, "{job}");

    // A recompute runs the same steps to the same end, so the visit owes none: the skips stand
    // as findings on the review queue instead.
    let (status, detail) = crate::common::get_json_with_token(
        &app,
        &format!("/api/collection_events/{event_id}/detail"),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{detail}");
    assert_eq!(detail["recompute"], "current", "{detail}");
    // Each skip says what it lacks: B's script raised, and C waits on B.
    let finding_of = |code: &str| {
        detail["cells"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["parameter_code"] == code)
            .map(|c| c["finding"].clone())
            .unwrap_or_else(|| panic!("{code} carries a finding: {detail}"))
    };
    let b_finding = finding_of("ChainPB");
    assert_eq!(b_finding["cause"], "error", "{b_finding}");
    let c_finding = finding_of("ChainPC");
    assert_eq!(c_finding["cause"], "upstream", "{c_finding}");
    assert_eq!(c_finding["waits_on"], "chain_b", "{c_finding}");

    // The review queue carries each absent output with the reason its step did not run, and it
    // is still there when the job row that counted them is gone.
    crate::common::exec(
        &db,
        &format!("DELETE FROM reprocessing_jobs WHERE id = '{job_id}'"),
    )
    .await;
    let findings =
        serde_json::Value::Array(e2e::pending_event_findings(&app, &admin, &site_id).await);
    let holds = findings.as_array().unwrap();
    assert_eq!(holds.len(), 2, "one finding per absent output: {findings}");
    let b_hold = holds
        .iter()
        .find(|h| h["tool"] == "chain_b")
        .expect("the raising step is reported");
    assert_eq!(b_hold["kind"], "skipped_output");
    assert_eq!(b_hold["parameter_code"], "ChainPB");
    assert!(
        b_hold["expected"]["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("division by zero"),
        "the script's message survives the job row: {b_hold}"
    );
    let c_hold = holds
        .iter()
        .find(|h| h["tool"] == "chain_c")
        .expect("the step below it is reported");
    assert_eq!(c_hold["kind"], "skipped_output");
    assert_eq!(c_hold["parameter_code"], "ChainPC");

    // The audit adds nothing over a slot the executor already explained.
    let (status, audit) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/event_audit",
        &json!({ "site_id": site_id }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "audit enqueue: {audit}");
    let audit_job = audit["job_id"].as_str().expect("job id").to_string();
    assert_eq!(
        e2e::poll_job(&app, &admin, &audit_job, 60).await,
        "completed"
    );
    let after = serde_json::Value::Array(e2e::pending_event_findings(&app, &admin, &site_id).await);
    assert_eq!(
        after.as_array().unwrap().len(),
        2,
        "the audit duplicated the executor's findings: {after}"
    );
}

/// Scenario: a calculation whose formula reads a standard curve is run at a visit with a curve
/// chosen by hand, and one of its inputs is corrected afterwards.
///
/// Expected behaviour: the recompute the correction fires supplies the same curve slot again, so
/// the corrected output is rewritten under the curve instead of the formula being skipped for want
/// of it. Formula-engined, so the arithmetic runs in-process and the story needs no R runner.
///
/// Run: cargo test --test e2e a_corrected_input_recomputes -- --test-threads=1
#[tokio::test]
#[serial]
async fn a_corrected_input_recomputes_its_output_under_the_curve_the_run_chose() {
    use sea_orm::ConnectionTrait;
    if !crate::common::profile::Service::Keycloak
        .require("a_corrected_input_recomputes_its_output_under_the_curve_the_run_chose")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    for sql in [
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name LIKE 'curvechain\\_%'",
        "DELETE FROM tool_script_activations a USING tool_scripts s \
          WHERE a.tool_script_id = s.id AND s.name LIKE 'curvechain\\_%'",
        "DELETE FROM tool_script_versions v USING tool_scripts s \
          WHERE v.tool_script_id = s.id AND s.name LIKE 'curvechain\\_%'",
        "DELETE FROM calculation_formulas f USING tool_scripts s \
          WHERE f.tool_script_id = s.id AND s.name LIKE 'curvechain\\_%'",
        "DELETE FROM tool_scripts WHERE name LIKE 'curvechain\\_%'",
    ] {
        crate::common::exec(&db, sql).await;
    }
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let project_id = e2e::create_project(&app, &admin, "Curve Project", "curvep", false).await;
    let site_id = e2e::create_site(&app, &admin, &project_id, "Curve Site", "curves").await;
    let input = e2e::create_parameter(&app, &admin, "CurveIn", "Curve in", "ppm").await;

    let (status, created) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tool_scripts",
        &json!({ "name": "curvechain_corr", "label": "Curve corr", "engine": "formula" }),
        &admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create the calculation: {created}"
    );
    let script_id = e2e::id_of(&created);

    let (status, saved) = crate::common::save_formula_set(
        &app,
        &admin,
        &script_id,
        json!([{
            "code": "CurveOut", "name": "Curve out", "units": "ppm",
            "formula": "CurveIn * curve_slope + curve_intercept",
            "curve_slot": "vaisala", "ordinal": 1,
        }]),
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "save the formula ({status}): {saved}"
    );
    let output = minted_output(&db, "CurveOut").await;
    e2e::declare_site_slots(
        &db,
        &app,
        &admin,
        &site_id,
        "curve_group",
        &[input.as_str(), output.as_str()],
    )
    .await;

    // The curve the operator picks for this run: y = 2x + 1.
    let analyser = e2e::create_sensor(&app, &admin, &input, "curve-analyser").await;
    let curve_id = uuid::Uuid::new_v4().to_string();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, name, slope, intercept) \
             VALUES ('{curve_id}', '{analyser}', 'Vaisala plate', 2.0, 1.0)"
        ),
    )
    .await;

    kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    kc::grant_project(&db, &kc::keycloak_user_id("river1").await, &project_id).await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;

    let measure = |value: f64, replace: bool| {
        let app = app.clone();
        let river = river.clone();
        let site_id = site_id.clone();
        let input = input.clone();
        async move {
            let mut body = json!({
                "site_id": site_id,
                "readings": [{ "parameter_id": input, "value": value, "time": EVENT_TIME }],
            });
            if replace {
                body["mode"] = json!("replace");
            }
            let (status, resp) = crate::common::post_checked_grab(&app, &body, &river).await;
            assert_eq!(status, 200, "save CurveIn: {resp}");
        }
    };
    let stored_output = || {
        let db = db.clone();
        let site_id = site_id.clone();
        let output = output.clone();
        async move {
            db.query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT COALESCE(calibrated_value, raw_value) AS value FROM readings \
                     WHERE site_id = '{site_id}' AND parameter_id = '{output}' \
                       AND withdrawn_at IS NULL ORDER BY replicate_index LIMIT 1"
                ),
            ))
            .await
            .expect("read the output")
            .and_then(|r| r.try_get::<Option<f64>>("", "value").expect("value"))
        }
    };

    // The input is entered, then the calculation is run by hand with the curve and saved: a
    // curve slot is filled only by the person running it (M300), so this is the run that fixes
    // which curve the visit's corrected value is made with.
    measure(10.0, false).await;
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    let (status, run) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/curvechain_corr/calculate",
        &json!({
            "site_id": site_id,
            "collected_at": EVENT_TIME,
            "vaisala": { "standard_curve_id": curve_id },
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "run with the curve: {run}");
    assert_eq!(run["results"]["CurveOut"], 21.0, "10 * 2 + 1: {run}");
    let (status, resp) = crate::common::post_checked_grab(
        &app,
        &json!({
            "site_id": site_id,
            "tool_run_id": run["run_id"],
            "mode": "replace",
            "readings": [{ "parameter_id": output, "value": 21.0,
                            "time": EVENT_TIME, "output": "CurveOut" }],
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "save the corrected output: {resp}");
    assert_eq!(stored_output().await, Some(21.0));

    // The input was mistyped: 20, not 10. The chain fires, and the recompute has to name the
    // curve the run chose or the formula reading it is skipped and 21 stands.
    measure(20.0, true).await;
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    e2e::drain_jobs(&db, 60).await;
    assert_eq!(
        stored_output().await,
        Some(41.0),
        "20 * 2 + 1, the correction recomputed under the run's own curve"
    );

    let findings =
        serde_json::Value::Array(e2e::pending_event_findings(&app, &admin, &site_id).await);
    assert!(
        findings.as_array().unwrap().is_empty(),
        "the curve was supplied, so no output was skipped: {findings}"
    );
}
