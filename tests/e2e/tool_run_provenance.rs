//! S1, tool lifecycle with server-side provenance (story catalog: ../archived-documentation/PLAN.md).
//!
//! Scenario: a member runs an analytical tool and saves its outputs at a site. The calculation
//! itself is the stored record (`tool_runs`), the save names that run, and the provenance blob on
//! the samples rows is built by the server from the stored run: actor from the authenticated
//! caller, inputs, constants and curves as the engine resolved them, and the saved mapping from
//! run outputs to catalog parameters. Nothing in the blob is client-authored.
//!
//! `site_inputs` resolved from the site's own properties and site_parameter provisioning on a
//! first save are covered here too, along with the refusals: a value the run did not produce, a
//! client-authored blob, a run saved onto another visit, and an aggregate output saved as a
//! measurement while its replicates still save.

use serde_json::json;
use serial_test::serial;

use crate::common::keycloak as kc;
use crate::common::{e2e, tracks};

/// The intern's entry sits at its own instant, clear of the member's save.
const INTERN_TIME: &str = "2025-06-15T09:00:00Z";

#[tokio::test]
#[serial]
async fn a_calculation_is_a_stored_run_and_the_save_carries_its_blob() {
    if !crate::common::profile::Service::Keycloak
        .require("a_calculation_is_a_stored_run_and_the_save_carries_its_blob")
        .await
    {
        return;
    }
    if !crate::common::profile::Service::ToolsRunner
        .require("a_calculation_is_a_stored_run_and_the_save_carries_its_blob")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_grab_track(&app, &admin).await;
    let parameter_id = track.parameter_id("TrkGrabDoc").to_string();
    let intern = e2e::member(&db, &track.project_id, "intern1", "riverdata-intern").await;
    let river = e2e::member(&db, &track.project_id, "river1", "riverdata-river").await;

    // The calculation, as the member. The response names the stored run.
    let (status, tool) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &json!({ "DOC": [120.0, 125.0, 118.0] }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "calculate ({status}): {tool}");
    let run_id = tool["run_id"].as_str().expect("run_id on the response");
    let doc_avg = tool["results"]["DOC_avg_ppb"].as_f64().expect("DOC avg");
    // The replicates the run consumed are what is stored; its avg and sd are statistics of that
    // group and are served from `samples`.
    let replicates: Vec<serde_json::Value> = [120.0, 125.0, 118.0]
        .iter()
        .enumerate()
        .map(|(i, v)| {
            json!({
                "parameter_id": parameter_id,
                "value": v,
                "time": "2025-06-15T11:00:00Z",
                "replicate_index": i as i16,
                "input": "DOC",
            })
        })
        .collect();

    // The stored run carries the calculating actor, resolved from the JWT, not the request.
    let run_row = {
        use sea_orm::{ConnectionTrait, Statement};
        db.query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT created_by, outputs->>'DOC_avg_ppb' AS avg FROM tool_runs \
                 WHERE id = '{run_id}'"
            ),
        ))
        .await
        .expect("query tool_runs")
        .expect("the calculation stored a run row")
    };
    let calculated_by: String = run_row.try_get("", "created_by").unwrap();
    assert!(
        !calculated_by.is_empty() && !calculated_by.starts_with("token:"),
        "the run records the Keycloak identity that calculated: {calculated_by}"
    );

    // An intern's entry lands, unverified, and is held for review rather than published. It goes
    // at its own instant: an entry already standing at this one is what the member's save would
    // have to displace, which an intern's entry may not be used to force.
    let intern_readings: Vec<serde_json::Value> = replicates
        .iter()
        .map(|r| {
            let mut r = r.clone();
            r["time"] = json!(INTERN_TIME);
            r
        })
        .collect();
    let (status, entered) = crate::common::post_json_parse_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": track.site_id,
            "tool_run_id": run_id,
            "readings": intern_readings,
        }),
        &intern,
    )
    .await;
    assert_eq!(
        status, 200,
        "an intern enters field data ({status}): {entered}"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT count(*) FROM readings WHERE site_id = '{}' \
                 AND parameter_id = '{parameter_id}' AND time = '{INTERN_TIME}' \
                 AND unverified IS TRUE",
                track.site_id
            ),
        )
        .await,
        3,
        "every replicate the intern entered is unverified"
    );
    assert_eq!(
        e2e::count(
            &db,
            "SELECT count(*) FROM replicate_audit_holds \
             WHERE kind = 'unverified_entry' AND status IN ('pending', 'deferred')",
        )
        .await,
        1,
        "the entry opens one review hold"
    );

    let save_body = json!({
        "site_id": track.site_id,
        "tool_run_id": run_id,
        "readings": replicates,
    });
    let (status, saved) =
        crate::common::post_json_parse_with_token(&app, "/api/grab_samples", &save_body, &river)
            .await;
    assert_eq!(status, 200, "the member saves ({status}): {saved}");
    assert_eq!(saved["inserted"], 3);

    // The blob on the reading is the stored run, plus the saved mapping and both actors.
    let blob = {
        use sea_orm::{ConnectionTrait, Statement};
        db.query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT provenance FROM readings WHERE site_id = '{}' \
                 AND parameter_id = '{parameter_id}' AND provenance IS NOT NULL LIMIT 1",
                track.site_id
            ),
        ))
        .await
        .expect("query readings")
        .expect("the save stored its readings")
        .try_get::<serde_json::Value>("", "provenance")
        .expect("the reading carries the blob")
    };
    assert_eq!(blob["tool"], "doc");
    assert_eq!(blob["run_id"], run_id);
    assert_eq!(blob["calculated_by"], calculated_by);
    assert_eq!(
        blob["saved_by"], calculated_by,
        "the same member calculated and saved"
    );
    assert_eq!(blob["inputs"]["DOC"][0], 120.0);
    assert_eq!(blob["outputs"]["DOC_avg_ppb"], doc_avg);
    assert_eq!(blob["saved_inputs"]["DOC"], parameter_id);
    assert!(
        blob["tool_version"]["content_hash"].as_str().is_some(),
        "the blob pins the script version: {blob}"
    );

    // The blob reads back through the provenance endpoint, so an auditor never needs the database.
    let (status, record) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/readings/provenance?site_id={}&parameter_id={parameter_id}&time=2025-06-15T11:00:00Z",
            track.site_id
        ),
        &river,
    )
    .await;
    assert_eq!(status, 200, "provenance ({status}): {record}");
    assert_eq!(
        record["records"][0]["computation"]["provenance"]["run_id"], run_id,
        "the served record carries the run: {record}"
    );
}

/// Expected behaviour: the link between a save and a run is verified, so a claim the run does not
/// back is refused, and the retired client-authored blob is refused by name rather than dropped.
#[tokio::test]
#[serial]
async fn a_forged_or_edited_tool_link_is_refused_at_the_gate() {
    if !crate::common::profile::Service::Keycloak
        .require("a_forged_or_edited_tool_link_is_refused_at_the_gate")
        .await
    {
        return;
    }
    if !crate::common::profile::Service::ToolsRunner
        .require("a_forged_or_edited_tool_link_is_refused_at_the_gate")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_grab_track(&app, &admin).await;
    let parameter_id = track.parameter_id("TrkGrabDoc").to_string();
    let river = e2e::member(&db, &track.project_id, "river1", "riverdata-river").await;

    let (status, tool) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &json!({ "DOC": [120.0, 125.0, 118.0] }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "calculate ({status}): {tool}");
    let run_id = tool["run_id"].as_str().expect("run_id");
    let doc_avg = tool["results"]["DOC_avg_ppb"].as_f64().expect("DOC avg");

    // A value the run did not produce.
    let (status, resp) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": track.site_id,
            "tool_run_id": run_id,
            "readings": [{
                "parameter_id": parameter_id,
                "value": doc_avg + 0.1,
                "time": "2025-06-15T11:00:00Z",
                "output": "DOC_avg_ppb",
            }],
        }),
        &river,
    )
    .await;
    assert_eq!(status, 400, "an edited value is refused: {resp}");

    // The retired field: a client-authored blob is refused, not silently dropped.
    let (status, resp) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": track.site_id,
            "provenance": { "tool": "doc", "outputs": {} },
            "readings": [{
                "parameter_id": parameter_id,
                "value": doc_avg,
                "time": "2025-06-15T11:00:00Z",
            }],
        }),
        &river,
    )
    .await;
    assert_eq!(status, 422, "a client-authored blob is refused: {resp}");
}

/// S1's Phase 3 half: a manifest declares `site_inputs` that the engine resolves from the site
/// at calculate time; a site missing a declared property is refused naming it, and the resolved
/// value lands in the run and its blob.
#[tokio::test]
#[serial]
async fn a_site_input_resolves_from_the_site_and_a_missing_property_is_refused() {
    if !crate::common::profile::Service::Keycloak
        .require("a_site_input_resolves_from_the_site_and_a_missing_property_is_refused")
        .await
    {
        return;
    }
    if !crate::common::profile::Service::ToolsRunner
        .require("a_site_input_resolves_from_the_site_and_a_missing_property_is_refused")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::exec(
        &db,
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name LIKE 'station_%'",
    )
    .await;
    crate::common::exec(
        &db,
        "DELETE FROM tool_script_activations WHERE tool_script_id IN \
         (SELECT id FROM tool_scripts WHERE name LIKE 'station_%')",
    )
    .await;
    crate::common::exec(
        &db,
        "DELETE FROM tool_script_versions WHERE tool_script_id IN \
         (SELECT id FROM tool_scripts WHERE name LIKE 'station_%')",
    )
    .await;
    crate::common::exec(&db, "DELETE FROM tool_scripts WHERE name LIKE 'station_%'").await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_grab_track(&app, &admin).await;
    let echo_param =
        crate::common::e2e::create_parameter(&app, &admin, "AltEcho", "Altitude echo", "m").await;
    crate::common::e2e::declare_site_slots(
        &db,
        &app,
        &admin,
        &track.site_id,
        "alt_echo_group",
        &[(echo_param.as_str(), "output")],
    )
    .await;
    crate::common::e2e::author_tool(
        &app,
        &admin,
        "station_echo",
        "tool <- function(inputs, constants, curves) list(alt_echo = inputs$altitude_m * 1)",
        json!({
            "label": "Station echo",
            "params": [{ "name": "altitude_m", "label": "Altitude", "kind": "number", "required": true }],
            "station_inputs": [{ "property": "altitude_m" }],
            "outputs": [{ "key": "alt_echo", "label": "Echo", "suggested_parameter_code": "AltEcho" }],
        }),
        json!({ "name": "echoes", "inputs": { "altitude_m": 100.0 }, "expected": { "alt_echo": 100.0 } }),
    )
    .await;
    let river = e2e::member(&db, &track.project_id, "river1", "riverdata-river").await;

    // No site context at all: the declaration is enforced, naming what is missing.
    let (status, resp) = crate::common::post_json_with_token(
        &app,
        "/api/tools/station_echo/calculate",
        &json!({}),
        &river,
    )
    .await;
    assert_eq!(status, 400, "{resp}");
    assert!(resp.contains("altitude_m"), "{resp}");

    // The track site has no altitude: refused naming the property, not a generic missing-field.
    let (status, resp) = crate::common::post_json_with_token(
        &app,
        "/api/tools/station_echo/calculate",
        &json!({ "site_id": track.site_id }),
        &river,
    )
    .await;
    assert_eq!(status, 400, "{resp}");
    assert!(
        resp.contains("altitude_m") && resp.contains("no value"),
        "{resp}"
    );

    let (status, patched) = crate::common::put_json_with_token(
        &app,
        &format!("/api/sites/{}", track.site_id),
        &json!({ "altitude_m": 512.0 }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{patched}");

    let (status, tool) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/station_echo/calculate",
        &json!({ "site_id": track.site_id }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{tool}");
    assert_eq!(tool["results"]["alt_echo"], 512.0);
    assert_eq!(tool["site_inputs"][0]["property"], "altitude_m");
    assert_eq!(tool["site_inputs"][0]["value"], 512.0);

    // The save carries the resolution into the blob's context.
    let (status, saved) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": track.site_id,
            "tool_run_id": tool["run_id"],
            "readings": [{ "parameter_id": echo_param, "value": 512.0,
                            "time": "2025-06-15T12:00:00Z", "output": "alt_echo" }],
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{saved}");
    let blob = {
        use sea_orm::{ConnectionTrait, Statement};
        db.query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT provenance FROM readings WHERE site_id = '{}' \
                 AND parameter_id = '{echo_param}' AND provenance IS NOT NULL LIMIT 1",
                track.site_id
            ),
        ))
        .await
        .unwrap()
        .expect("the save stored its reading")
        .try_get::<serde_json::Value>("", "provenance")
        .unwrap()
    };
    assert_eq!(blob["context"]["site_inputs"][0]["property"], "altitude_m");
    assert_eq!(blob["context"]["site_inputs"][0]["value"], 512.0);
    assert_eq!(
        blob["inputs"]["altitude_m"], 512.0,
        "the resolved value is a recorded input"
    );

    // A typed value wins over the stored property, and then nothing is recorded as resolved.
    let (status, tool) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/station_echo/calculate",
        &json!({ "site_id": track.site_id, "altitude_m": 300.0 }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{tool}");
    assert_eq!(tool["results"]["alt_echo"], 300.0);
    assert!(
        tool["site_inputs"].as_array().is_none_or(Vec::is_empty),
        "{tool}"
    );

    // A resolved value is kind-checked exactly like a typed one: a manifest wiring a text
    // property (the site's name) into a number param is refused naming the mismatch, never
    // handed to the runner.
    crate::common::e2e::author_tool(
        &app,
        &admin,
        "station_name_echo",
        "tool <- function(inputs, constants, curves) list(out = inputs$alt)",
        json!({
            "label": "Station name echo",
            "params": [{ "name": "alt", "label": "Alt", "kind": "number", "required": true }],
            "site_inputs": [{ "property": "name", "param": "alt" }],
            "outputs": [],
        }),
        json!({ "name": "echoes", "inputs": { "alt": 1.0 }, "expected": { "out": 1.0 } }),
    )
    .await;
    let (status, resp) = crate::common::post_json_with_token(
        &app,
        "/api/tools/station_name_echo/calculate",
        &json!({ "site_id": track.site_id }),
        &river,
    )
    .await;
    assert_eq!(status, 400, "{resp}");
    assert!(
        resp.contains("is not a number") && resp.contains("'name'"),
        "the mismatch is named: {resp}"
    );
}

/// S1's Phase 3 half, under Q98: a site declares which calculations apply to it by holding their
/// output slots, so a save landing on a slot the site does not carry is refused and the same save
/// succeeds once the slot exists. A mint here would create the declaration it is checked against.
#[tokio::test]
#[serial]
async fn a_save_needs_the_site_to_carry_the_slot() {
    if !crate::common::profile::Service::Keycloak
        .require("a_save_needs_the_site_to_carry_the_slot")
        .await
    {
        return;
    }
    if !crate::common::profile::Service::ToolsRunner
        .require("a_save_needs_the_site_to_carry_the_slot")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_grab_track(&app, &admin).await;
    // A catalog parameter matching the doc tool's replicates param, assigned to no site.
    let doc_param = crate::common::e2e::create_parameter(&app, &admin, "DOC", "DOC", "ppb").await;
    let river = e2e::member(&db, &track.project_id, "river1", "riverdata-river").await;

    let (status, tool) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &json!({ "DOC": [120.0, 125.0, 118.0] }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{tool}");
    let save = json!({
        "site_id": track.site_id,
        "tool_run_id": tool["run_id"],
        "readings": [{ "parameter_id": doc_param, "value": 120.0, "replicate_index": 0,
                        "time": "2025-06-15T13:00:00Z", "input": "DOC" }],
    });
    let (status, refused) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &save, &river).await;
    assert_eq!(
        status, 400,
        "a save may not mint the declaration it is checked against: {refused}"
    );
    assert!(
        refused.contains("parameter_groups"),
        "the refusal names the flow that adds the slot: {refused}"
    );
    let minted = crate::common::e2e::count(
        &db,
        &format!(
            "SELECT COUNT(*)::bigint AS n FROM site_parameters \
             WHERE site_id = '{}' AND parameter_id = '{doc_param}'",
            track.site_id
        ),
    )
    .await;
    assert_eq!(minted, 0, "the refused save minted nothing");

    // Declaring the slot is what makes the calculation apply here.
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type, \
             is_active, is_public, needs_review, created_at) \
             VALUES (gen_random_uuid(), '{}', '{doc_param}', 'DOC', '', TRUE, FALSE, FALSE, NOW())",
            track.site_id
        ),
    )
    .await;

    let (status, saved) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &save, &river).await;
    assert_eq!(
        status, 200,
        "the declared slot admits the same save: {saved}"
    );

    // The reading landed attributed, and a second save reuses the slot.
    let attributed = crate::common::e2e::count(
        &db,
        &format!(
            "SELECT COUNT(*)::bigint AS n FROM readings \
             WHERE site_id = '{}' AND parameter_id = '{doc_param}'",
            track.site_id
        ),
    )
    .await;
    assert_eq!(attributed, 1);

    let mut second = save.clone();
    second["mode"] = json!("replace");
    let (status, saved) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &second, &river).await;
    assert_eq!(status, 200, "{saved}");
    let slots = crate::common::e2e::count(
        &db,
        &format!(
            "SELECT COUNT(*)::bigint AS n FROM site_parameters \
             WHERE site_id = '{}' AND parameter_id = '{doc_param}'",
            track.site_id
        ),
    )
    .await;
    assert_eq!(slots, 1, "the second save reuses the declared slot");
}

/// A run records the visit it was calculated for. Saving it at another station, or onto another
/// instant, would file numbers computed from one visit's context as another's.
#[tokio::test]
#[serial]
async fn a_run_cannot_be_saved_onto_another_visit() {
    if !crate::common::profile::Service::Keycloak
        .require("a_run_cannot_be_saved_onto_another_visit")
        .await
    {
        return;
    }
    if !crate::common::profile::Service::ToolsRunner
        .require("a_run_cannot_be_saved_onto_another_visit")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::exec(
        &db,
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name LIKE 'context_%'",
    )
    .await;
    crate::common::exec(
        &db,
        "DELETE FROM tool_script_activations WHERE tool_script_id IN \
         (SELECT id FROM tool_scripts WHERE name LIKE 'context_%')",
    )
    .await;
    crate::common::exec(
        &db,
        "DELETE FROM tool_script_versions WHERE tool_script_id IN \
         (SELECT id FROM tool_scripts WHERE name LIKE 'context_%')",
    )
    .await;
    crate::common::exec(&db, "DELETE FROM tool_scripts WHERE name LIKE 'context_%'").await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_grab_track(&app, &admin).await;
    let other_site =
        crate::common::e2e::create_site(&app, &admin, &track.project_id, "Other Station", "othr")
            .await;
    let echo_param =
        crate::common::e2e::create_parameter(&app, &admin, "CtxEcho", "Context echo", "m").await;
    // Both stations carry the slot, so the refusals below are the run's own site and visit rather
    // than the Q98 gate.
    let group_id = crate::common::e2e::declare_site_slots(
        &db,
        &app,
        &admin,
        &track.site_id,
        "ctx_echo_group",
        &[(echo_param.as_str(), "output")],
    )
    .await;
    let (status, applied) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sites/{other_site}/parameter_groups"),
        &json!({ "group_id": group_id }),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "apply the group to the other station: {applied}"
    );
    crate::common::e2e::author_tool(
        &app,
        &admin,
        "context_echo",
        "tool <- function(inputs, constants, curves) list(ctx_echo = inputs$altitude_m * 1)",
        json!({
            "label": "Context echo",
            "params": [{ "name": "altitude_m", "label": "Altitude", "kind": "number", "required": true }],
            "site_inputs": [{ "property": "altitude_m" }],
            "outputs": [{ "key": "ctx_echo", "label": "Echo", "suggested_parameter_code": "CtxEcho" }],
        }),
        json!({ "name": "echoes", "inputs": { "altitude_m": 100.0 }, "expected": { "ctx_echo": 100.0 } }),
    )
    .await;
    let river = e2e::member(&db, &track.project_id, "river1", "riverdata-river").await;

    let (status, patched) = crate::common::put_json_with_token(
        &app,
        &format!("/api/sites/{}", track.site_id),
        &json!({ "altitude_m": 512.0 }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{patched}");

    const VISIT: &str = "2025-06-15T12:00:00Z";
    let (status, tool) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/context_echo/calculate",
        &json!({ "site_id": track.site_id, "collected_at": VISIT }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{tool}");
    let run_id = tool["run_id"].clone();

    let reading = |time: &str| json!([{ "parameter_id": echo_param, "value": 512.0, "time": time, "output": "ctx_echo" }]);

    let (status, resp) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({ "site_id": other_site, "tool_run_id": run_id, "readings": reading(VISIT) }),
        &river,
    )
    .await;
    assert_eq!(status, 400, "another station must be refused: {resp}");
    assert!(
        resp.contains(&track.site_id),
        "the refusal names the site the run was calculated for: {resp}"
    );

    let (status, resp) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": track.site_id,
            "tool_run_id": run_id,
            "readings": reading("2025-06-16T12:00:00Z"),
        }),
        &river,
    )
    .await;
    assert_eq!(status, 400, "another visit must be refused: {resp}");
    assert!(
        resp.contains("2025-06-15") && resp.contains("2025-06-16"),
        "the refusal names both instants: {resp}"
    );

    let (status, saved) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({ "site_id": track.site_id, "tool_run_id": run_id, "readings": reading(VISIT) }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "the run's own visit still saves: {saved}");
}

/// An output that reduces a replicates param is a statistic of the group, not a measurement.
/// Saving it would put a mean in the readings the `samples` trigger takes a mean over.
#[tokio::test]
#[serial]
async fn an_aggregate_output_cannot_be_saved_as_a_measurement() {
    if !crate::common::profile::Service::Keycloak
        .require("an_aggregate_output_cannot_be_saved_as_a_measurement")
        .await
    {
        return;
    }
    if !crate::common::profile::Service::ToolsRunner
        .require("an_aggregate_output_cannot_be_saved_as_a_measurement")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_grab_track(&app, &admin).await;
    let parameter_id = track.parameter_id("TrkGrabDoc").to_string();
    let river = e2e::member(&db, &track.project_id, "river1", "riverdata-river").await;

    const AT: &str = "2025-06-15T14:00:00Z";
    let values = [120.0, 125.0, 118.0];
    let (status, tool) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &json!({ "DOC": values }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{tool}");
    let run_id = tool["run_id"].clone();
    let doc_avg = tool["results"]["DOC_avg_ppb"].as_f64().expect("avg");

    for output in ["DOC_avg_ppb", "DOC_sd_ppb"] {
        let (status, refused) = crate::common::post_json_with_token(
            &app,
            "/api/grab_samples",
            &json!({
                "site_id": track.site_id,
                "tool_run_id": run_id,
                "readings": [{ "parameter_id": parameter_id, "time": AT, "output": output,
                                "value": tool["results"][output] }],
            }),
            &river,
        )
        .await;
        assert_eq!(status, 400, "{output} must be refused: {refused}");
        assert!(refused.contains(output), "the refusal names it: {refused}");
    }

    // The replicates the statistics reduce still save, and the sample mean is the run's average.
    let readings: Vec<serde_json::Value> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            json!({ "parameter_id": parameter_id, "time": AT, "value": v,
                     "replicate_index": i as i16, "input": "DOC" })
        })
        .collect();
    let (status, saved) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({ "site_id": track.site_id, "tool_run_id": run_id, "readings": readings }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "the replicates save: {saved}");

    let mean = {
        use sea_orm::{ConnectionTrait, Statement};
        db.query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT mean FROM samples WHERE site_id = '{}' \
                 AND parameter_id = '{parameter_id}' AND collected_at = '{AT}'",
                track.site_id
            ),
        ))
        .await
        .unwrap()
        .expect("the save formed a sample")
        .try_get::<f64>("", "mean")
        .unwrap()
    };
    assert!(
        (mean - doc_avg).abs() < 1e-9,
        "the served mean is the run's average: {mean} vs {doc_avg}"
    );
}
