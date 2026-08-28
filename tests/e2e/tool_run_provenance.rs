//! S1, tool lifecycle with server-side provenance (PLAN.md story catalog).
//!
//! Scenario: a member runs an analytical tool and saves its outputs at a station. The calculation
//! itself is the stored record (`tool_runs`), the save names that run, and the provenance blob on
//! the samples rows is built by the server from the stored run: actor from the authenticated
//! caller, inputs, constants and curves as the engine resolved them, and the saved mapping from
//! run outputs to catalog parameters. Nothing in the blob is client-authored.
//!
//! The Phase 3 halves of S1 (manifest `station_inputs` resolved from site properties,
//! `event_inputs` from the shared collection event, site_parameter auto-provisioning on first
//! save) are encoded below as ignored tests gating that phase.

use serde_json::json;
use serial_test::serial;

use crate::common::keycloak as kc;
use crate::common::tracks;

async fn member(
    db: &sea_orm::DatabaseConnection,
    project_id: &str,
    user: &str,
    role: &str,
) -> String {
    kc::ensure_realm_user(user, user, &[role]).await;
    kc::grant_project(db, &kc::keycloak_user_id(user).await, project_id).await;
    kc::get_keycloak_jwt(user, user).await
}

#[tokio::test]
#[serial]
async fn a_calculation_is_a_stored_run_and_the_save_carries_its_blob() {
    if !kc::require_keycloak_or_skip("a_calculation_is_a_stored_run_and_the_save_carries_its_blob")
        .await
    {
        return;
    }
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_calculation_is_a_stored_run_and_the_save_carries_its_blob",
    )
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
    let intern = member(&db, &track.project_id, "intern1", "riverdata-intern").await;
    let river = member(&db, &track.project_id, "river1", "riverdata-river").await;

    // The calculation, as the member. The response names the stored run.
    let (status, tool) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &json!({ "DOC_rep_1": 120.0, "DOC_rep_2": 125.0, "DOC_rep_3": 118.0 }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "calculate ({status}): {tool}");
    let run_id = tool["run_id"].as_str().expect("run_id on the response");
    let doc_avg = tool["results"]["DOC_avg_ppb"].as_f64().expect("DOC avg");

    // The stored run carries the calculating actor, resolved from the JWT, not the request.
    let run_row = {
        use sea_orm::{ConnectionTrait, Statement};
        db.query_one(Statement::from_string(
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

    // An intern reads tools but cannot write data; the save gate refuses them.
    let save_body = json!({
        "site_id": track.site_id,
        "tool_run_id": run_id,
        "readings": [{
            "parameter_id": parameter_id,
            "value": doc_avg,
            "time": "2025-06-15T11:00:00Z",
            "output": "DOC_avg_ppb",
        }],
    });
    let (status, refused) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &save_body, &intern).await;
    assert_eq!(status, 403, "an intern cannot save: {refused}");

    let (status, saved) =
        crate::common::post_json_parse_with_token(&app, "/api/grab_samples", &save_body, &river)
            .await;
    assert_eq!(status, 200, "the member saves ({status}): {saved}");
    assert_eq!(saved["inserted"], 1);

    // The blob on the samples row is the stored run, plus the saved mapping and both actors.
    let blob = {
        use sea_orm::{ConnectionTrait, Statement};
        db.query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT provenance FROM samples WHERE site_id = '{}' \
                 AND parameter_id = '{parameter_id}'",
                track.site_id
            ),
        ))
        .await
        .expect("query samples")
        .expect("the save formed a sample")
        .try_get::<serde_json::Value>("", "provenance")
        .expect("the sample carries the blob")
    };
    assert_eq!(blob["tool"], "doc");
    assert_eq!(blob["run_id"], run_id);
    assert_eq!(blob["calculated_by"], calculated_by);
    assert_eq!(
        blob["saved_by"], calculated_by,
        "the same member calculated and saved"
    );
    assert_eq!(blob["inputs"]["DOC_rep_1"], 120.0);
    assert_eq!(blob["outputs"]["DOC_avg_ppb"], doc_avg);
    assert_eq!(blob["saved"]["DOC_avg_ppb"], parameter_id);
    assert!(
        blob["tool_version"]["content_hash"].as_str().is_some(),
        "the blob pins the script version: {blob}"
    );

    // The blob reads back through the samples API, so an auditor never needs the database.
    let (status, listed) = crate::common::get_json_with_token(&app, "/api/samples", &river).await;
    assert_eq!(status, 200, "samples list ({status}): {listed}");
    let row = listed
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["parameter_id"] == parameter_id))
        .unwrap_or_else(|| panic!("saved sample listed: {listed}"));
    assert_eq!(row["provenance"]["run_id"], run_id);
}

/// Expected behaviour: the link between a save and a run is verified, so a claim the run does not
/// back is refused, and the retired client-authored blob is refused by name rather than dropped.
#[tokio::test]
#[serial]
async fn a_forged_or_edited_tool_link_is_refused_at_the_gate() {
    if !kc::require_keycloak_or_skip("a_forged_or_edited_tool_link_is_refused_at_the_gate").await {
        return;
    }
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_forged_or_edited_tool_link_is_refused_at_the_gate",
    )
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
    let river = member(&db, &track.project_id, "river1", "riverdata-river").await;

    let (status, tool) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &json!({ "DOC_rep_1": 120.0, "DOC_rep_2": 125.0, "DOC_rep_3": 118.0 }),
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

/// S1's Phase 3 half: a manifest declares `station_inputs` that the engine resolves from the site
/// at calculate time; a site missing a declared property is refused naming it, and the resolved
/// value lands in the run and its blob.
#[tokio::test]
#[serial]
async fn a_station_input_resolves_from_the_site_and_a_missing_property_is_refused() {
    if !kc::require_keycloak_or_skip(
        "a_station_input_resolves_from_the_site_and_a_missing_property_is_refused",
    )
    .await
    {
        return;
    }
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_station_input_resolves_from_the_site_and_a_missing_property_is_refused",
    )
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
    let river = member(&db, &track.project_id, "river1", "riverdata-river").await;

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
    assert_eq!(tool["station_inputs"][0]["property"], "altitude_m");
    assert_eq!(tool["station_inputs"][0]["value"], 512.0);

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
        db.query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT provenance FROM samples WHERE site_id = '{}' \
                 AND parameter_id = '{echo_param}'",
                track.site_id
            ),
        ))
        .await
        .unwrap()
        .expect("the save formed a sample")
        .try_get::<serde_json::Value>("", "provenance")
        .unwrap()
    };
    assert_eq!(blob["context"]["station_inputs"][0]["property"], "altitude_m");
    assert_eq!(blob["context"]["station_inputs"][0]["value"], 512.0);
    assert_eq!(blob["inputs"]["altitude_m"], 512.0, "the resolved value is a recorded input");

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
    assert!(tool["station_inputs"].as_array().is_none_or(Vec::is_empty), "{tool}");

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
            "station_inputs": [{ "property": "name", "param": "alt" }],
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

/// S1's Phase 3 half: a first save to a catalog parameter the site does not carry provisions the
/// site_parameter instead of refusing the save.
#[tokio::test]
#[serial]
async fn a_first_save_provisions_the_site_parameter() {
    if !kc::require_keycloak_or_skip("a_first_save_provisions_the_site_parameter").await {
        return;
    }
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_first_save_provisions_the_site_parameter",
    )
    .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_grab_track(&app, &admin).await;
    // A catalog parameter matching doc's DOC_avg_ppb output code, assigned to no site.
    let doc_param = crate::common::e2e::create_parameter(&app, &admin, "DOC", "DOC", "ppb").await;
    let river = member(&db, &track.project_id, "river1", "riverdata-river").await;

    let (status, tool) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &json!({ "DOC_rep_1": 120.0, "DOC_rep_2": 125.0, "DOC_rep_3": 118.0 }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "{tool}");
    let doc_avg = tool["results"]["DOC_avg_ppb"].as_f64().expect("avg");

    let save = json!({
        "site_id": track.site_id,
        "tool_run_id": tool["run_id"],
        "readings": [{ "parameter_id": doc_param, "value": doc_avg,
                        "time": "2025-06-15T13:00:00Z", "output": "DOC_avg_ppb" }],
    });
    let (status, saved) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &save, &river).await;
    assert_eq!(status, 200, "the first save provisions the slot: {saved}");

    let (needs_review, count) = {
        use sea_orm::{ConnectionTrait, Statement};
        let row = db
            .query_one(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT needs_review, (SELECT COUNT(*)::bigint FROM site_parameters \
                       WHERE site_id = '{0}' AND parameter_id = '{doc_param}') AS n \
                     FROM site_parameters WHERE site_id = '{0}' AND parameter_id = '{doc_param}'",
                    track.site_id
                ),
            ))
            .await
            .unwrap()
            .expect("the site_parameter was minted");
        (
            row.try_get::<bool>("", "needs_review").unwrap(),
            row.try_get::<i64>("", "n").unwrap(),
        )
    };
    assert!(needs_review, "a mechanical slot awaits review");
    assert_eq!(count, 1);

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
    assert_eq!(slots, 1, "the second save reuses the provisioned slot");
}
