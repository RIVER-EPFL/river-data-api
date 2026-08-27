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

/// S1's Phase 3 half: a manifest declares `station_inputs` (elevation, latitude, …) that the
/// engine resolves from the site at calculate time; a site missing a declared property is refused
/// naming it, and the resolved values land in the run and its blob.
#[tokio::test]
#[serial]
#[ignore = "BLOCKED: manifest station_inputs land with Phase 3 (collection events and the tool workflow)"]
async fn a_station_input_resolves_from_the_site_and_a_missing_property_is_refused() {
    unimplemented!(
        "author a tool whose manifest declares station_inputs [altitude_m]; calculate against a \
         site with no altitude -> 400 naming altitude_m; set the altitude -> the run's blob \
         records the resolved value under station_inputs"
    );
}

/// S1's Phase 3 half: a first save to a catalog parameter the site does not carry provisions the
/// site_parameter instead of refusing the save.
#[tokio::test]
#[serial]
#[ignore = "BLOCKED: site_parameter auto-provisioning on first tool save lands with Phase 3"]
async fn a_first_save_provisions_the_site_parameter() {
    unimplemented!(
        "save a tool output to a catalog parameter not yet assigned to the site -> the \
         site_parameter is created (needs_review), the reading lands attributed, and the second \
         save reuses it"
    );
}
