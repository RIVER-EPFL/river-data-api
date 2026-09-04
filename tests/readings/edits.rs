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
    let decision = json!({ "kind": "value_correction", "value": 40.0 });

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
            "decision": { "kind": "value_correction", "value": 41.0 },
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
