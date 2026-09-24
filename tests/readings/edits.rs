//! The edit primitive (Q8, M60): a value a tool run produced is reopened in its tool; a value
//! nothing computed is corrected in place, previewed first and reversible after.
//!
//! Run: cargo test --test readings edits -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const AT: &str = "2025-06-15T10:00:00Z";

struct Fixture {
    app: axum::Router,
    token: String,
    db: DatabaseConnection,
    stream: Uuid,
}

async fn setup(values: &[f64]) -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let stream = crate::common::sensor_lifecycle::create_paired_stream(
        &db,
        "edits-temp",
        crate::common::PARAM_S1_TEMP_ID,
    )
    .await;
    let sample_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO samples (id, site_id, parameter_id, collected_at) \
             VALUES ('{sample_id}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{AT}')"
        ),
    )
    .await;
    for (i, v) in values.iter().enumerate() {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, \
                 replicate_index, measurement_type, sample_id) \
                 VALUES ('{stream}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{AT}', {v}, {i}, \
                         'spot', '{sample_id}')"
            ),
        )
        .await;
    }
    Fixture {
        app,
        token,
        db,
        stream,
    }
}

/// A tool run standing behind replicate 0, as the grab save stores one.
async fn attach_run(f: &Fixture) -> Uuid {
    let run_id = Uuid::new_v4();
    crate::common::exec(
        &f.db,
        &format!(
            "INSERT INTO tool_runs (id, tool_name, tool_version, inputs, constants, curves, \
                 outputs, created_by, context, source) \
             VALUES ('{run_id}', 'doc', '{{}}'::jsonb, '{{\"DOC\": [1.0, 2.0]}}'::jsonb, \
                     '{{}}'::jsonb, '[]'::jsonb, '{{\"doc_avg\": 1.5}}'::jsonb, 'tester', \
                     '{{\"site_id\": \"{SITE1_ID}\", \"collected_at\": \"{AT}\"}}'::jsonb, \
                     'interactive')"
        ),
    )
    .await;
    crate::common::exec(
        &f.db,
        &format!(
            "UPDATE readings SET provenance = '{{\"run_id\": \"{run_id}\"}}'::jsonb \
             WHERE stream_id = '{}' AND time = '{AT}' AND replicate_index = 0",
            f.stream
        ),
    )
    .await;
    run_id
}

fn one_key(stream: Uuid, index: i16) -> serde_json::Value {
    json!({ "keys": [{ "stream_id": stream, "time": AT, "replicate_index": index }] })
}

async fn post(f: &Fixture, uri: &str, body: &serde_json::Value) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(&f.app, uri, body, &f.token).await
}

async fn stored(f: &Fixture, index: i16) -> (f64, bool) {
    let row =
        f.db.query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT raw_value, withdrawn_at IS NOT NULL AS withdrawn FROM readings \
                 WHERE stream_id = '{}' AND time = '{AT}' AND replicate_index = {index}",
                f.stream
            ),
        ))
        .await
        .unwrap()
        .expect("the seeded reading");
    (
        row.try_get("", "raw_value").unwrap(),
        row.try_get("", "withdrawn").unwrap(),
    )
}

async fn sample_mean(f: &Fixture) -> Option<f64> {
    f.db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT s.mean FROM samples s \
             JOIN readings r ON r.sample_id = s.id \
             WHERE r.stream_id = '{}' AND r.time = '{AT}' LIMIT 1",
            f.stream
        ),
    ))
    .await
    .unwrap()
    .and_then(|r| r.try_get::<Option<f64>>("", "mean").ok())
    .flatten()
}

/// Preview an edit and commit it under the preview's id.
async fn preview_and_commit(
    f: &Fixture,
    selection: &serde_json::Value,
    decision: &serde_json::Value,
) -> (u16, serde_json::Value) {
    let body = json!({ "selection": selection, "decision": decision });
    let (status, preview) = post(f, "/api/readings/edits/preview", &body).await;
    assert_eq!(status, 200, "{preview}");
    let mut commit = body.clone();
    commit["preview_id"] = preview["preview_id"].clone();
    post(f, "/api/readings/edits", &commit).await
}

/// Screen corrected values, returning the check a correction of them names.
async fn seasonal_check(f: &Fixture, values: &[f64]) -> String {
    let values: Vec<serde_json::Value> = values
        .iter()
        .map(|v| json!({ "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": v }))
        .collect();
    let (status, body) = post(
        f,
        "/api/readings/seasonal_check",
        &json!({ "site_id": SITE1_ID, "time": AT, "values": values }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body["check_id"].as_str().expect("check id").to_string()
}

#[tokio::test]
#[serial]
async fn a_preview_shows_the_write_s_own_arithmetic_and_changes_nothing() {
    let f = setup(&[10.0, 12.0, 14.0]).await;
    assert_eq!(sample_mean(&f).await, Some(12.0));

    let body = json!({
        "selection": one_key(f.stream, 0),
        "decision": { "kind": "value_correction", "value": 40.0 }
    });
    let (status, preview) = post(&f, "/api/readings/edits/preview", &body).await;
    assert_eq!(status, 200, "{preview}");
    assert_eq!(preview["rows"][0]["before"]["raw_value"], 10.0);
    assert_eq!(preview["rows"][0]["after"]["raw_value"], 40.0);
    assert_eq!(
        preview["samples"][0]["before"]["mean"], 12.0,
        "the group's statistics as they stand: {preview}"
    );
    assert_eq!(
        preview["samples"][0]["after"]["mean"], 22.0,
        "and as the trigger recomputes them: {preview}"
    );
    assert!(
        !preview["not_previewed"].as_array().unwrap().is_empty(),
        "the preview says what it does not cover"
    );

    assert_eq!(stored(&f, 0).await.0, 10.0, "the preview wrote nothing");
    assert_eq!(sample_mean(&f).await, Some(12.0));
}

#[tokio::test]
#[serial]
async fn a_commit_is_held_to_the_preview_of_itself_and_is_reversible() {
    let f = setup(&[10.0, 12.0, 14.0]).await;
    let selection = one_key(f.stream, 0);
    let check_id = seasonal_check(&f, &[40.0]).await;
    let decision = json!({ "kind": "value_correction", "value": 40.0, "check_id": check_id });

    let (status, preview) = post(
        &f,
        "/api/readings/edits/preview",
        &json!({ "selection": selection, "decision": decision }),
    )
    .await;
    assert_eq!(status, 200, "{preview}");
    let preview_id = preview["preview_id"].as_str().unwrap().to_string();

    // Committing without a preview, and committing a different edit under this preview's id, are
    // both refused: the id is the selection and the decision.
    let (status, body) = post(
        &f,
        "/api/readings/edits",
        &json!({ "selection": selection, "decision": decision }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = post(
        &f,
        "/api/readings/edits",
        &json!({
            "selection": selection,
            "decision": { "kind": "value_correction", "value": 41.0, "check_id": check_id },
            "preview_id": preview_id
        }),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(stored(&f, 0).await.0, 10.0, "nothing was written");

    let (status, committed) = post(
        &f,
        "/api/readings/edits",
        &json!({ "selection": selection, "decision": decision, "preview_id": preview_id }),
    )
    .await;
    assert_eq!(status, 200, "{committed}");
    assert_eq!(committed["rows_decided"], 1);
    assert_eq!(stored(&f, 0).await.0, 40.0);
    assert_eq!(
        sample_mean(&f).await,
        Some(22.0),
        "the number the preview showed is the number that landed"
    );

    let decision_id = committed["decision_ids"][0].as_str().unwrap().to_string();
    let (status, rolled) = post(
        &f,
        &format!("/api/readings/edits/{decision_id}/rollback"),
        &json!({}),
    )
    .await;
    assert_eq!(status, 200, "{rolled}");
    assert_eq!(stored(&f, 0).await.0, 10.0, "the prior value is restored");
    assert_eq!(sample_mean(&f).await, Some(12.0));
}

#[tokio::test]
#[serial]
async fn a_value_a_tool_run_produced_is_reopened_rather_than_corrected() {
    let f = setup(&[10.0, 12.0, 14.0]).await;
    let run_id = attach_run(&f).await;

    let (status, inspected) = post(
        &f,
        "/api/readings/edits/inspect",
        &json!({ "selection": one_key(f.stream, 0) }),
    )
    .await;
    assert_eq!(status, 200, "{inspected}");
    let options = inspected["rows"][0]["options"].as_array().unwrap();
    assert!(options.iter().any(|o| o == "reopen_run"), "{inspected}");
    assert!(
        !options.iter().any(|o| o == "value_correction"),
        "a tool's number is not corrected here: {inspected}"
    );
    assert_eq!(inspected["rows"][0]["tool_run_id"], run_id.to_string());
    for field in ["site_id", "parameter_id"] {
        assert!(
            inspected["rows"][0][field].is_string(),
            "the row names the slot a detach addresses: {inspected}"
        );
    }

    // And the route refuses the in-place correction rather than quietly taking it.
    let (status, refused) = post(
        &f,
        "/api/readings/edits/preview",
        &json!({
            "selection": one_key(f.stream, 0),
            "decision": { "kind": "value_correction", "value": 40.0 }
        }),
    )
    .await;
    assert_eq!(status, 400, "{refused}");

    // Taking the slot off its calculation is the one override Q117 kept: the correction is then
    // offered here, and the way back with it.
    crate::common::exec(
        &f.db,
        &format!(
            "INSERT INTO reading_decisions (stream_id, time, kind, old, new, actor, origin) \
             VALUES ('{}', '{AT}', 'detach', '{{}}'::jsonb, '{{\"owner\": \"manual\"}}'::jsonb, \
                     'tester', 'manual')",
            f.stream
        ),
    )
    .await;
    let (status, inspected) = post(
        &f,
        "/api/readings/edits/inspect",
        &json!({ "selection": one_key(f.stream, 0) }),
    )
    .await;
    assert_eq!(status, 200, "{inspected}");
    let options = inspected["rows"][0]["options"].as_array().unwrap();
    assert!(
        options.iter().any(|o| o == "value_correction"),
        "{inspected}"
    );
    assert!(options.iter().any(|o| o == "return"), "{inspected}");
    assert!(!options.iter().any(|o| o == "reopen_run"), "{inspected}");
    assert!(
        !options.iter().any(|o| o == "detach"),
        "it is already detached: {inspected}"
    );

    let (status, previewed) = post(
        &f,
        "/api/readings/edits/preview",
        &json!({
            "selection": one_key(f.stream, 0),
            "decision": { "kind": "value_correction", "value": 40.0 }
        }),
    )
    .await;
    assert_eq!(status, 200, "{previewed}");

    // The run reloads in the shape the tool's calculate body takes, with its visit context.
    let (status, reload) = crate::common::get_json_with_token(
        &f.app,
        &format!("/api/tool_runs/{run_id}/reload"),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "{reload}");
    assert_eq!(reload["tool"], "doc");
    assert_eq!(reload["body"]["DOC"], json!([1.0, 2.0]));
    assert_eq!(reload["body"]["site_id"], SITE1_ID);
    assert_eq!(reload["body"]["collected_at"], AT);
}

#[tokio::test]
#[serial]
async fn a_withdrawal_through_the_primitive_is_a_stamp_a_reassert_lifts() {
    let f = setup(&[10.0, 12.0]).await;
    let selection = one_key(f.stream, 1);
    for (kind, withdrawn) in [("withdraw", true), ("reassert", false)] {
        let decision = json!({ "kind": kind, "reason": "checked against the field sheet" });
        let (status, preview) = post(
            &f,
            "/api/readings/edits/preview",
            &json!({ "selection": selection, "decision": decision }),
        )
        .await;
        assert_eq!(status, 200, "{preview}");
        let (status, committed) = post(
            &f,
            "/api/readings/edits",
            &json!({
                "selection": selection,
                "decision": decision,
                "preview_id": preview["preview_id"]
            }),
        )
        .await;
        assert_eq!(status, 200, "{committed}");
        assert_eq!(stored(&f, 1).await.1, withdrawn, "after {kind}");
        assert_eq!(stored(&f, 1).await.0, 12.0, "nothing deletes");
    }
}

/// Scenario: a block of cells is corrected in one save, each cell to a different number.
///
/// Expected behaviour: one preview and one commit cover the block, every value lands on its own
/// key, and the whole block is one decision set that rolls back together.
#[tokio::test]
#[serial]
async fn a_block_corrected_to_different_values_is_one_decision_set() {
    let f = setup(&[10.0, 12.0, 14.0]).await;
    let selection = json!({
        "keys": [
            { "stream_id": f.stream, "time": AT, "replicate_index": 0, "value": 20.0 },
            { "stream_id": f.stream, "time": AT, "replicate_index": 2, "value": 24.0 },
        ]
    });
    let check_id = seasonal_check(&f, &[20.0, 24.0]).await;
    let decision =
        json!({ "kind": "value_correction", "reason": "pasted block", "check_id": check_id });

    let (status, preview) = post(
        &f,
        "/api/readings/edits/preview",
        &json!({ "selection": selection, "decision": decision }),
    )
    .await;
    assert_eq!(status, 200, "preview: {preview}");
    assert_eq!(
        preview["rows"].as_array().map(Vec::len),
        Some(2),
        "the preview covers both cells: {preview}"
    );
    assert_eq!(
        stored(&f, 0).await.0,
        10.0,
        "a preview leaves the stored value alone"
    );

    let (status, committed) = post(
        &f,
        "/api/readings/edits",
        &json!({
            "selection": selection,
            "decision": decision,
            "preview_id": preview["preview_id"],
        }),
    )
    .await;
    assert_eq!(status, 200, "commit: {committed}");
    assert_eq!(stored(&f, 0).await.0, 20.0);
    assert_eq!(stored(&f, 2).await.0, 24.0);
    assert_eq!(
        stored(&f, 1).await.0,
        12.0,
        "a key the block did not name is untouched"
    );

    let set_id = committed["set_id"].as_str().expect("one set for the block");
    let (status, rolled) = post(
        &f,
        &format!("/api/readings/edits/sets/{set_id}/rollback"),
        &json!({}),
    )
    .await;
    assert_eq!(status, 200, "rollback: {rolled}");
    assert_eq!(stored(&f, 0).await.0, 10.0, "the whole block came back");
    assert_eq!(stored(&f, 2).await.0, 14.0);
}

/// A key with a value beside one without is a selection nobody meant, so it is refused rather
/// than half applied.
#[tokio::test]
#[serial]
async fn a_block_mixing_valued_and_unvalued_keys_is_refused() {
    let f = setup(&[10.0, 12.0]).await;
    let (status, body) = post(
        &f,
        "/api/readings/edits/preview",
        &json!({
            "selection": { "keys": [
                { "stream_id": f.stream, "time": AT, "replicate_index": 0, "value": 20.0 },
                { "stream_id": f.stream, "time": AT, "replicate_index": 1 },
            ]},
            "decision": { "kind": "value_correction", "reason": "half a block" },
        }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
}

/// Scenario: one cell of a block is rolled back on its own, then the whole set is.
/// Expected behaviour: the set rollback names only the decisions still live, so the row already
/// undone is not undone twice and the count reports what it actually moved.
#[tokio::test]
#[serial]
async fn a_set_rollback_skips_a_decision_already_rolled_back() {
    let f = setup(&[10.0, 12.0, 14.0]).await;
    let selection = json!({
        "keys": [
            { "stream_id": f.stream, "time": AT, "replicate_index": 0, "value": 20.0 },
            { "stream_id": f.stream, "time": AT, "replicate_index": 2, "value": 24.0 },
        ]
    });
    let check_id = seasonal_check(&f, &[20.0, 24.0]).await;
    let decision =
        json!({ "kind": "value_correction", "reason": "pasted block", "check_id": check_id });

    let (status, preview) = post(
        &f,
        "/api/readings/edits/preview",
        &json!({ "selection": selection, "decision": decision }),
    )
    .await;
    assert_eq!(status, 200, "preview: {preview}");
    let (status, committed) = post(
        &f,
        "/api/readings/edits",
        &json!({
            "selection": selection,
            "decision": decision,
            "preview_id": preview["preview_id"],
        }),
    )
    .await;
    assert_eq!(status, 200, "commit: {committed}");

    let ids = committed["decision_ids"]
        .as_array()
        .expect("the commit names its decisions");
    assert_eq!(ids.len(), 2, "two cells, two decisions: {committed}");
    let first = ids[0].as_str().expect("a decision id");

    let (status, rolled_one) = post(
        &f,
        &format!("/api/readings/edits/{first}/rollback"),
        &json!({}),
    )
    .await;
    assert_eq!(status, 200, "single rollback: {rolled_one}");

    let set_id = committed["set_id"].as_str().expect("one set for the block");
    let (status, rolled) = post(
        &f,
        &format!("/api/readings/edits/sets/{set_id}/rollback"),
        &json!({}),
    )
    .await;
    assert_eq!(status, 200, "set rollback: {rolled}");
    assert_eq!(
        rolled["rolled_back"], 1,
        "only the decision still live is rolled back: {rolled}"
    );
    assert_eq!(stored(&f, 0).await.0, 10.0, "both cells are back");
    assert_eq!(stored(&f, 2).await.0, 14.0);
}

/// Scenario: a spot reading corrected by its instrument's curve is moved to another curve by id.
///
/// Expected behaviour: the edit is held to the admission every other curve writer applies. A curve
/// fitted on another instrument is refused at the preview and at the commit, and a second curve of
/// the reading's own instrument is accepted.
#[tokio::test]
#[serial]
async fn a_curve_edit_is_refused_a_curve_another_instrument_fitted() {
    let f = setup(&[10.0]).await;
    let own: Uuid =
        f.db.query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT sensor_id FROM readings WHERE stream_id = '{}' AND time = '{AT}'",
                f.stream
            ),
        ))
        .await
        .unwrap()
        .expect("the seeded reading")
        .try_get::<Option<Uuid>>("", "sensor_id")
        .unwrap()
        .expect("the reading names its instrument");
    let other = Uuid::new_v4();
    let (current, own_curve, foreign_curve) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    for sql in [
        format!(
            "INSERT INTO sensors (id, name, is_active) VALUES ('{other}', 'other analyser', true)"
        ),
        format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept) VALUES \
             ('{current}', '{own}', 1.5, 0.0), ('{own_curve}', '{own}', 2.0, 0.0), \
             ('{foreign_curve}', '{other}', 3.0, 0.0)"
        ),
        format!(
            "UPDATE readings SET standard_curve_id = '{current}' \
             WHERE stream_id = '{}' AND time = '{AT}'",
            f.stream
        ),
    ] {
        crate::common::exec(&f.db, &sql).await;
    }
    let edit = |curve: Uuid| {
        json!({
            "selection": one_key(f.stream, 0),
            "decision": { "kind": "curve", "target_id": curve }
        })
    };

    let (status, body) = post(&f, "/api/readings/edits/preview", &edit(foreign_curve)).await;
    assert_eq!(
        status, 400,
        "another instrument's curve is refused at the preview: {body}"
    );
    let mut committed = edit(foreign_curve);
    committed["preview_id"] = json!(Uuid::new_v4());
    let (status, body) = post(&f, "/api/readings/edits", &committed).await;
    assert_eq!(
        status, 400,
        "and at the commit, whatever preview it cites: {body}"
    );

    let (status, body) = post(&f, "/api/readings/edits/preview", &edit(own_curve)).await;
    assert_eq!(
        status, 200,
        "the reading's own instrument's curve is accepted: {body}"
    );
}

/// Scenario: a grab of raw 120 is served as 60 under curve A (slope 0.5), and a manager moves it to
/// curve B (slope 0.8).
///
/// Expected behaviour: the corrected value moves with the curve in the edit's own transaction, 96,
/// and the rollback puts back 60 the same way, so no sweep is left to reconcile a row whose value
/// and curve disagree.
#[tokio::test]
#[serial]
async fn a_curve_edit_recomposes_the_value_and_its_rollback_restores_it() {
    let f = setup(&[120.0]).await;
    let (curve_a, curve_b) = (Uuid::new_v4(), Uuid::new_v4());
    for sql in [
        format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept) \
             SELECT '{curve_a}', sensor_id, 0.5, 0.0 FROM readings WHERE stream_id = '{}'",
            f.stream
        ),
        format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept) \
             SELECT '{curve_b}', sensor_id, 0.8, 0.0 FROM readings WHERE stream_id = '{}'",
            f.stream
        ),
        format!(
            "UPDATE readings SET standard_curve_id = '{curve_a}', calibrated_value = 60.0 \
             WHERE stream_id = '{}'",
            f.stream
        ),
    ] {
        crate::common::exec(&f.db, &sql).await;
    }
    let served = || async {
        f.db.query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT calibrated_value FROM readings WHERE stream_id = '{}'",
                f.stream
            ),
        ))
        .await
        .unwrap()
        .expect("the seeded reading")
        .try_get::<Option<f64>>("", "calibrated_value")
        .unwrap()
    };

    let mut edit = json!({
        "selection": one_key(f.stream, 0),
        "decision": { "kind": "curve", "target_id": curve_b }
    });
    let (status, preview) = post(&f, "/api/readings/edits/preview", &edit).await;
    assert_eq!(status, 200, "{preview}");
    assert_eq!(
        preview["rows"][0]["after"]["calibrated_value"], 96.0,
        "the preview shows the value the new curve composes: {preview}"
    );
    edit["preview_id"] = preview["preview_id"].clone();
    let (status, recorded) = post(&f, "/api/readings/edits", &edit).await;
    assert_eq!(status, 200, "{recorded}");
    assert_eq!(served().await, Some(96.0), "120 × 0.8");

    let set_id = recorded["set_id"].as_str().expect("a set").to_string();
    let (status, body) = post(
        &f,
        &format!("/api/readings/edits/sets/{set_id}/rollback"),
        &json!({}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        served().await,
        Some(60.0),
        "120 × 0.5, restored with the curve"
    );
}

#[tokio::test]
#[serial]
async fn a_grab_correction_is_committed_only_under_a_check_that_screened_it() {
    let f = setup(&[10.0, 12.0, 14.0]).await;
    let selection = one_key(f.stream, 0);

    let (status, body) = preview_and_commit(
        &f,
        &selection,
        &json!({ "kind": "value_correction", "value": 12000.0 }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("seasonal_check"),
        "{body}"
    );
    assert_eq!(stored(&f, 0).await.0, 10.0, "nothing was written");

    let other = seasonal_check(&f, &[50.0]).await;
    let (status, body) = preview_and_commit(
        &f,
        &selection,
        &json!({ "kind": "value_correction", "value": 12000.0, "check_id": other }),
    )
    .await;
    assert_eq!(
        status, 409,
        "a check over another value does not cover this one: {body}"
    );
    assert_eq!(stored(&f, 0).await.0, 10.0);

    // Far outside the range, and still saved: the check warns, it does not block.
    let check_id = seasonal_check(&f, &[12000.0]).await;
    let (status, body) = preview_and_commit(
        &f,
        &selection,
        &json!({ "kind": "value_correction", "value": 12000.0, "check_id": check_id }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored(&f, 0).await.0, 12000.0);
}

#[tokio::test]
#[serial]
async fn a_correction_of_sensor_rows_needs_no_check() {
    let f = setup(&[]).await;
    crate::common::exec(
        &f.db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, \
             replicate_index, measurement_type) \
             VALUES ('{}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{AT}', 10, 0, 'continuous')",
            f.stream
        ),
    )
    .await;
    let (status, body) = preview_and_commit(
        &f,
        &one_key(f.stream, 0),
        &json!({ "kind": "value_correction", "value": 11.0 }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored(&f, 0).await.0, 11.0);
}

/// Expected behaviour: an admin overrides a calculated value in one act (Q263). The value is
/// replaced, the slot detached, and the provenance names the computed value it replaced; a
/// value no tool produced is refused, and a return restores the computed value.
#[tokio::test]
#[serial]
async fn an_admin_overrides_a_calculated_value_in_one_act() {
    if !crate::common::profile::Service::Keycloak
        .require("an_admin_overrides_a_calculated_value_in_one_act")
        .await
    {
        return;
    }
    let f = setup(&[10.0]).await;
    let admin_app = crate::common::keycloak::build_test_app_with_keycloak(f.db.clone()).await;
    let admin = crate::common::keycloak::get_keycloak_jwt("admin", "admin").await;
    let override_at = |value: f64| {
        json!({
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "time": AT,
            "value": value,
            "reason": "field log",
        })
    };
    let overridden = async |value: f64| {
        crate::common::post_json_parse_with_token(
            &admin_app,
            "/api/readings/override",
            &override_at(value),
            &admin,
        )
        .await
    };

    let (status, refused) = overridden(40.0).await;
    assert_eq!(
        status, 400,
        "no tool produced it, so it is corrected in place: {refused}"
    );

    attach_run(&f).await;
    let (status, done) = overridden(40.0).await;
    assert_eq!(status, 200, "{done}");
    assert_eq!(stored(&f, 0).await.0, 40.0);

    let (status, record) = crate::common::get_json_with_token(
        &f.app,
        &format!("/api/readings/provenance?stream_id={}&time={AT}", f.stream),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "{record}");
    let facet = &record["records"][0]["readings"][0]["overridden"];
    assert_eq!(facet["computed_value"], 10.0, "{record}");
    assert_eq!(facet["reason"], "field log");
    assert!(facet["by"].is_string(), "{record}");

    let (status, _) = overridden(50.0).await;
    assert_eq!(
        status, 409,
        "a detached value is corrected, not overridden again"
    );

    let (status, returned) = crate::common::post_json_parse_with_token(
        &admin_app,
        "/api/readings/return",
        &json!({ "site_id": SITE1_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": AT }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{returned}");
    assert_eq!(
        stored(&f, 0).await.0,
        10.0,
        "the return restores the computed value"
    );
    let (_, record) = crate::common::get_json_with_token(
        &f.app,
        &format!("/api/readings/provenance?stream_id={}&time={AT}", f.stream),
        &f.token,
    )
    .await;
    assert!(
        record["records"][0]["readings"][0]["overridden"].is_null(),
        "the calculation owns the value again: {record}"
    );
}

/// Scenario: one block corrects a reading on each of two parameters, as a pasted row of a visit
/// does, and the set is read before and after its rollback.
/// Expected behaviour: the set lists both decisions with the parameter each one's reading measures
/// and the value it moved, however many streams it reached, and reads as rolled back afterwards.
#[tokio::test]
#[serial]
async fn an_edit_set_lists_every_decision_it_recorded_with_its_parameter() {
    let f = setup(&[10.0]).await;
    let other = crate::common::sensor_lifecycle::create_paired_stream(
        &f.db,
        "edits-do",
        crate::common::PARAM_S1_DO_ID,
    )
    .await;
    crate::common::exec(
        &f.db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, \
             replicate_index, measurement_type) \
             VALUES ('{other}', '{SITE1_ID}', '{}', '{AT}', 8.0, 0, 'spot')",
            crate::common::GLOBAL_PARAM_DO_ID
        ),
    )
    .await;
    let selection = json!({
        "keys": [
            { "stream_id": f.stream, "time": AT, "replicate_index": 0, "value": 15.0 },
            { "stream_id": other, "time": AT, "replicate_index": 0, "value": 9.0 },
        ]
    });
    let (status, check) = post(
        &f,
        "/api/readings/seasonal_check",
        &json!({
            "site_id": SITE1_ID,
            "time": AT,
            "values": [
                { "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 15.0 },
                { "parameter_id": crate::common::GLOBAL_PARAM_DO_ID, "value": 9.0 },
            ],
        }),
    )
    .await;
    assert_eq!(status, 200, "{check}");
    let decision = json!({
        "kind": "value_correction",
        "reason": "pasted row",
        "check_id": check["check_id"],
    });
    let (status, preview) = post(
        &f,
        "/api/readings/edits/preview",
        &json!({ "selection": selection, "decision": decision }),
    )
    .await;
    assert_eq!(status, 200, "preview: {preview}");
    let (status, committed) = post(
        &f,
        "/api/readings/edits",
        &json!({
            "selection": selection,
            "decision": decision,
            "preview_id": preview["preview_id"],
        }),
    )
    .await;
    assert_eq!(status, 200, "commit: {committed}");
    let set_id = committed["set_id"].as_str().expect("one set for the block");
    let uri = format!("/api/readings/edits/sets/{set_id}");

    let (status, set) = crate::common::get_json_with_token(&f.app, &uri, &f.token).await;
    assert_eq!(status, 200, "{set}");
    assert_eq!(set["rolled_back_at"], serde_json::Value::Null);
    let members = set["members"].as_array().expect("the set's members");
    assert_eq!(members.len(), 2, "both streams are listed: {set}");
    let db = &f.db;
    let codes = |id: &'static str| async move {
        db.query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT code FROM parameters WHERE id = '{id}'"),
        ))
        .await
        .unwrap()
        .expect("a seeded parameter")
        .try_get::<String>("", "code")
        .unwrap()
    };
    let temp = codes(GLOBAL_PARAM_TEMP_ID).await;
    let dissolved = codes(crate::common::GLOBAL_PARAM_DO_ID).await;
    let member = |stream: Uuid| {
        members
            .iter()
            .find(|m| m["decision"]["stream_id"] == json!(stream))
            .unwrap_or_else(|| panic!("a member on stream {stream}: {set}"))
    };
    assert_eq!(member(f.stream)["parameter_code"], json!(temp));
    assert_eq!(member(f.stream)["decision"]["old"]["raw_value"], 10.0);
    assert_eq!(member(f.stream)["decision"]["new"]["raw_value"], 15.0);
    assert_eq!(member(other)["parameter_code"], json!(dissolved));
    assert_eq!(member(other)["decision"]["old"]["raw_value"], 8.0);
    assert_eq!(member(other)["decision"]["new"]["raw_value"], 9.0);

    let (status, rolled) = post(&f, &format!("{uri}/rollback"), &json!({})).await;
    assert_eq!(status, 200, "rollback: {rolled}");
    let (status, set) = crate::common::get_json_with_token(&f.app, &uri, &f.token).await;
    assert_eq!(status, 200, "{set}");
    assert!(set["rolled_back_at"].is_string(), "{set}");
    assert!(
        set["members"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["decision"]["rolled_back_by"].is_string()),
        "every member is rolled back: {set}"
    );

    let (status, _) = crate::common::get_json_with_token(
        &f.app,
        &format!("/api/readings/edits/sets/{}", Uuid::new_v4()),
        &f.token,
    )
    .await;
    assert_eq!(status, 404);
}
