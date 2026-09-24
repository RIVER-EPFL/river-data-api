//! Scenario: an author corrects a formula calculation twice, saves the earlier formula back, and
//! then corrects a constant the formula reads.
//!
//! Expected behaviour (Q186, Q256): a save mints one version whatever it touched, and every visit
//! the superseded version produced is recomputed onto the new one. Saving an earlier formula back
//! re-activates the version that holds it, and that rollback recomputes too. A constant carries no
//! versions: the closure says which calculations read it before the write, and every reading
//! naming it is recomputed with a ledger row naming the value it replaced.
//!
//! The ledger row is a `chain` decision, not a `formula_transition`: the chain executor is what
//! writes the recomputed value, and `formula_transition` belongs to the continuous recompute on a
//! stream (`readings/service.rs`, `Writer::DerivedRecompute`).
//!
//! The calculation is formula-engined, so the arithmetic runs in-process and the story needs no
//! R runner.
//!
//! Run: cargo test --test e2e formula_version_recompute -- --test-threads=1

use sea_orm::ConnectionTrait;
use serde_json::json;
use serial_test::serial;

use crate::common::e2e;
use crate::common::keycloak as kc;

/// The three visits the first version produces values at.
const VISITS: [&str; 3] = [
    "2025-06-15T09:00:00Z",
    "2025-06-22T09:00:00Z",
    "2025-06-29T09:00:00Z",
];
/// The visit taken after the second save, so the second version is the only one that produced it.
const LATER: &str = "2025-07-06T09:00:00Z";

/// The served output value at a visit, and the version the reading names.
async fn output_at(
    db: &sea_orm::DatabaseConnection,
    site_id: &str,
    parameter_id: &str,
    at: &str,
) -> (Option<f64>, Option<String>) {
    let row = db
        .query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COALESCE(r.calibrated_value, r.raw_value) AS value, \
                        r.provenance -> 'tool_version' ->> 'script_version_id' AS version \
                   FROM readings r \
                  WHERE r.site_id = '{site_id}' AND r.parameter_id = '{parameter_id}' \
                    AND r.time = '{at}' AND r.withdrawn_at IS NULL \
                  ORDER BY r.replicate_index LIMIT 1"
            ),
        ))
        .await
        .expect("read the output")
        .unwrap_or_else(|| panic!("an output is stored at {at}"));
    (
        row.try_get::<Option<f64>>("", "value").expect("value"),
        row.try_get::<Option<String>>("", "version")
            .expect("version"),
    )
}

/// The version a calculation's nth version row carries, by version number.
async fn version_id(db: &sea_orm::DatabaseConnection, script_id: &str, version_no: i32) -> String {
    e2e::scalar(
        db,
        &format!(
            "SELECT id::text FROM tool_script_versions \
              WHERE tool_script_id = '{script_id}' AND version_no = {version_no}"
        ),
    )
    .await
}

/// Every ledger row written against the output at a visit, newest first: its kind, the value it
/// replaced, and whether the run behind the value changed.
async fn ledger_at(
    db: &sea_orm::DatabaseConnection,
    site_id: &str,
    parameter_id: &str,
    at: &str,
) -> Vec<(String, Option<f64>, bool)> {
    let rows = db
        .query_all_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT d.kind, \
                        (d.old ->> 'raw_value')::double precision AS was, \
                        d.old ->> 'run_id' IS DISTINCT FROM d.new ->> 'run_id' AS reran \
                   FROM reading_decisions d \
                   JOIN readings r ON r.stream_id = d.stream_id AND r.time = d.time \
                                  AND r.replicate_index = d.replicate_index \
                  WHERE r.site_id = '{site_id}' AND r.parameter_id = '{parameter_id}' \
                    AND r.time = '{at}' \
                  ORDER BY d.at DESC"
            ),
        ))
        .await
        .expect("read the ledger");
    rows.into_iter()
        .map(|r| {
            (
                r.try_get::<String>("", "kind").expect("kind"),
                r.try_get::<Option<f64>>("", "was").expect("was"),
                r.try_get::<bool>("", "reran").expect("reran"),
            )
        })
        .collect()
}

#[tokio::test]
#[serial]
async fn every_version_change_moves_what_the_replaced_version_stored() {
    if !crate::common::profile::Service::Keycloak
        .require("every_version_change_moves_what_the_replaced_version_stored")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    // The seeded calculations survive cleanup as reference data, so this fixture removes its own.
    for sql in [
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name LIKE 'fver\\_%'",
        "DELETE FROM tool_script_activations a USING tool_scripts s \
          WHERE a.tool_script_id = s.id AND s.name LIKE 'fver\\_%'",
        "DELETE FROM tool_script_versions v USING tool_scripts s \
          WHERE v.tool_script_id = s.id AND s.name LIKE 'fver\\_%'",
        "DELETE FROM calculation_formulas f USING tool_scripts s \
          WHERE f.tool_script_id = s.id AND s.name LIKE 'fver\\_%'",
        "DELETE FROM tool_scripts WHERE name LIKE 'fver\\_%'",
        "DELETE FROM constants WHERE name = 'fver_factor'",
    ] {
        crate::common::exec(&db, sql).await;
    }
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let project_id = e2e::create_project(&app, &admin, "Fver Project", "fverp", false).await;
    let site_id = e2e::create_site(&app, &admin, &project_id, "Fver Site", "fvers").await;
    let input = e2e::create_parameter(&app, &admin, "FverIn", "Fver in", "ppb").await;

    let (status, constant) = crate::common::post_json_parse_with_token(
        &app,
        "/api/constants",
        &json!({ "name": "fver_factor", "value": 1.0, "units": "", "description": "scale" }),
        &admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create the constant: {constant}"
    );
    let constant_id = e2e::id_of(&constant);

    let (status, created) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tool_scripts",
        &json!({ "name": "fver_scale", "label": "Fver scale", "engine": "formula" }),
        &admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create the calculation: {created}"
    );
    let script_id = e2e::id_of(&created);

    // The whole set in one request, which is what a save is: one version comes out of it (Q186).
    let save = |formula: &'static str, formula_id: Option<String>| {
        let app = app.clone();
        let admin = admin.clone();
        let script_id = script_id.clone();
        async move {
            let mut entry = json!({
                "code": "FverOut",
                "name": "Fver out",
                "units": "ppb",
                "formula": formula,
                "ordinal": 1,
            });
            if let Some(id) = formula_id {
                entry["id"] = json!(id);
            }
            let (status, saved) = crate::common::post_json_parse_with_token(
                &app,
                &format!("/api/tool_scripts/{script_id}/formulas"),
                &json!({ "formulas": [entry] }),
                &admin,
            )
            .await;
            assert!((200..300).contains(&status), "save ({status}): {saved}");
            saved
        }
    };

    let saved = save("FverIn * 2 * fver_factor", None).await;
    assert_eq!(
        saved["created"], 1,
        "the first save writes the formula: {saved}"
    );
    assert_eq!(saved["version_no"], 1, "and mints one version: {saved}");
    assert_eq!(
        saved["migrated"], false,
        "a first save supersedes nothing: {saved}"
    );
    let first_version = saved["version_id"].as_str().expect("a version").to_string();

    // The calculation mints its own output parameter (Q191), so the slot is declared after the
    // save that created it, not before.
    let output = e2e::scalar(
        &db,
        "SELECT id::text FROM parameters WHERE lower(code) = lower('FverOut')",
    )
    .await;
    let formula_id = e2e::scalar(
        &db,
        &format!("SELECT id::text FROM calculation_formulas WHERE tool_script_id = '{script_id}'"),
    )
    .await;
    e2e::declare_site_slots(
        &db,
        &app,
        &admin,
        &site_id,
        "fver_group",
        &[input.as_str(), output.as_str()],
    )
    .await;

    kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    kc::grant_project(&db, &kc::keycloak_user_id("river1").await, &project_id).await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;

    let measure = |at: &'static str| {
        let app = app.clone();
        let river = river.clone();
        let site_id = site_id.clone();
        let input = input.clone();
        async move {
            let (status, resp) = crate::common::post_json_with_token(
                &app,
                "/api/grab_samples",
                &json!({
                    "site_id": site_id,
                    "readings": [{ "parameter_id": input, "value": 10.0, "time": at }],
                }),
                &river,
            )
            .await;
            assert_eq!(status, 200, "save FverIn at {at}: {resp}");
        }
    };

    for at in VISITS {
        measure(at).await;
    }
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    for at in VISITS {
        assert_eq!(
            output_at(&db, &site_id, &output, at).await,
            (Some(20.0), Some(first_version.clone())),
            "10 * 2 * 1 under the first version, at {at}"
        );
    }

    // --- A correction ---
    // A coefficient correction saved as a new version moves the three stored values onto it.
    let saved = save("FverIn * 3 * fver_factor", Some(formula_id.clone())).await;
    assert_eq!(saved["updated"], 1, "the formula it named: {saved}");
    assert_eq!(saved["version_no"], 2, "one save, one version: {saved}");
    assert_eq!(
        saved["migrated"], true,
        "the save enqueues the migration: {saved}"
    );
    let second_version = saved["version_id"].as_str().expect("a version").to_string();
    e2e::drain_jobs(&db, 120).await;
    for at in VISITS {
        assert_eq!(
            output_at(&db, &site_id, &output, at).await,
            (Some(30.0), Some(second_version.clone())),
            "10 * 3 * 1, recomputed onto the second version, at {at}"
        );
    }

    // A measurement taken afterwards runs against the new version.
    measure(LATER).await;
    assert!(e2e::wait_for_jobs_by_trigger(&db, "event_recompute", 60).await);
    assert_eq!(
        output_at(&db, &site_id, &output, LATER).await,
        (Some(30.0), Some(second_version.clone())),
        "10 * 3 * 1 under the second version"
    );

    // --- A second correction ---
    // The reach is every visit the superseded version produced, which is now all four.
    let saved = save("FverIn * 4 * fver_factor", Some(formula_id.clone())).await;
    assert_eq!(saved["version_no"], 3, "one save, one version: {saved}");
    assert_eq!(saved["migrated"], true, "{saved}");
    let third_version = version_id(&db, &script_id, 3).await;
    assert_eq!(saved["version_id"], json!(third_version));
    e2e::drain_jobs(&db, 120).await;
    for at in VISITS.iter().chain([&LATER]) {
        assert_eq!(
            output_at(&db, &site_id, &output, at).await,
            (Some(40.0), Some(third_version.clone())),
            "10 * 4 * 1, recomputed onto the third version, at {at}"
        );
    }

    // The move is in the ledger, naming the value it replaced and the run that replaced it.
    let moved = ledger_at(&db, &site_id, &output, LATER).await;
    let (kind, was, reran) = moved
        .first()
        .cloned()
        .expect("the migration wrote a ledger row");
    assert_eq!(
        kind, "chain",
        "the chain executor is what recomputed it: {moved:?}"
    );
    assert_eq!(
        was,
        Some(30.0),
        "the value the new version replaced: {moved:?}"
    );
    assert!(reran, "and the run behind it moved: {moved:?}");

    // --- The rollback ---
    // The second formula saved back is the second version again, and the values move back to it.
    let saved = save("FverIn * 3 * fver_factor", Some(formula_id.clone())).await;
    assert_eq!(saved["version_no"], 2, "the version holding it: {saved}");
    assert_eq!(saved["migrated"], true, "a rollback recomputes too: {saved}");
    e2e::drain_jobs(&db, 120).await;
    for at in VISITS.iter().chain([&LATER]) {
        assert_eq!(
            output_at(&db, &site_id, &output, at).await,
            (Some(30.0), Some(second_version.clone())),
            "10 * 3 * 1, back on the second version, at {at}"
        );
    }

    // --- The constant ---
    // Asked before the write: which calculations read it, so an author editing it knows who it
    // reaches (Q170).
    let (status, closure) = crate::common::get_json_with_token(
        &app,
        &format!("/api/calculations/closure?constant_id={constant_id}"),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{closure}");
    let names: Vec<&str> = closure["calculations"]
        .as_array()
        .expect("calculations")
        .iter()
        .filter_map(|c| c["tool"].as_str())
        .collect();
    assert!(
        names.contains(&"fver_scale"),
        "the closure names the calculation that reads it: {closure}"
    );
    assert_eq!(
        closure["stored"]["readings"], 4,
        "and how much has already been computed from it: {closure}"
    );
    assert_eq!(closure["stored"]["visits"], 4, "{closure}");

    // A constant carries no versions, so the edit reaches every reading naming it.
    let (status, patched) = crate::common::put_json_with_token(
        &app,
        &format!("/api/constants/{constant_id}"),
        &json!({ "value": 2.0 }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{patched}");
    e2e::drain_jobs(&db, 120).await;

    for at in VISITS {
        assert_eq!(
            output_at(&db, &site_id, &output, at).await,
            (Some(60.0), Some(second_version.clone())),
            "10 * 3 * 2, recomputed under the active version, at {at}"
        );
    }
    assert_eq!(
        output_at(&db, &site_id, &output, LATER).await,
        (Some(60.0), Some(second_version.clone())),
        "10 * 3 * 2 at the later visit too"
    );
    let moved = ledger_at(&db, &site_id, &output, VISITS[0]).await;
    let (kind, was, reran) = moved
        .first()
        .cloned()
        .expect("the constant edit wrote a ledger row");
    assert_eq!(kind, "chain", "{moved:?}");
    assert_eq!(
        was,
        Some(30.0),
        "the value the constant edit replaced: {moved:?}"
    );
    assert!(reran, "and the run behind it moved: {moved:?}");
}
