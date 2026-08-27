//! The provenance blob on a tool save is built by the server from the stored `tool_runs` row:
//! a client cannot author it, a save cannot claim a run it did not use, a curve-consuming run
//! refuses a `standard_curve_id`, and the stored blob survives CRUD untouched.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_DO_ID, GLOBAL_PARAM_TURB_ID, SITE1_ID};

const GRAB_TIME: &str = "2025-03-01T10:00:00Z";

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

/// A stored run, as `/tools/{name}/calculate` would have written it.
async fn mint_run(
    db: &DatabaseConnection,
    tool: &str,
    outputs: serde_json::Value,
    curves: serde_json::Value,
) -> Uuid {
    let id = Uuid::new_v4();
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO tool_runs (id, tool_name, tool_version, inputs, constants, curves, outputs, \
         created_by) VALUES ($1, $2, '{\"content_hash\": \"abc\"}', '{\"DO_rep_A\": 10.0}', \
         '{}', $3, $4, 'calculating@example.org')",
        [id.into(), tool.into(), curves.into(), outputs.into()],
    ))
    .await
    .unwrap();
    id
}

fn run_outputs() -> serde_json::Value {
    json!({ "DO_rep_A": 10.0, "DO_rep_B": 12.0, "Turb": 3.5 })
}

fn tool_save_body(run_id: Uuid) -> serde_json::Value {
    json!({
        "site_id": SITE1_ID,
        "tool_run_id": run_id,
        "readings": [
            { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 10.0, "time": GRAB_TIME, "output": "DO_rep_A" },
            { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 12.0, "time": GRAB_TIME, "output": "DO_rep_B" },
            { "parameter_id": GLOBAL_PARAM_TURB_ID, "value": 3.5, "time": GRAB_TIME, "output": "Turb" },
        ],
    })
}

async fn stored_blobs(db: &DatabaseConnection) -> Vec<(String, serde_json::Value)> {
    let rows = db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT parameter_id::text AS pid, provenance FROM samples \
                 WHERE site_id = '{SITE1_ID}' ORDER BY parameter_id"
            ),
        ))
        .await
        .unwrap();
    rows.iter()
        .map(|r| {
            (
                r.try_get::<String>("", "pid").unwrap(),
                r.try_get::<Option<serde_json::Value>>("", "provenance")
                    .unwrap()
                    .unwrap_or(serde_json::Value::Null),
            )
        })
        .collect()
}

#[tokio::test]
#[serial]
async fn a_tool_save_builds_the_blob_from_the_stored_run() {
    let (db, app, token) = setup().await;
    let run_id = mint_run(&db, "doc", run_outputs(), json!([])).await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &tool_save_body(run_id),
        &token,
    )
    .await;
    assert_eq!(status, 200, "tool save lands: {body}");

    let blobs = stored_blobs(&db).await;
    assert_eq!(blobs.len(), 2, "one samples row per parameter group");
    for (_, blob) in &blobs {
        assert_eq!(blob["tool"], "doc");
        assert_eq!(blob["run_id"], json!(run_id));
        assert_eq!(
            blob["outputs"], run_outputs(),
            "the blob's outputs are the stored run's, not the request's"
        );
        assert_eq!(
            blob["calculated_by"], "calculating@example.org",
            "the calculating actor was stamped at calculate time"
        );
        assert!(
            blob["saved_by"]
                .as_str()
                .is_some_and(|s| s.starts_with("token:")),
            "the saving actor comes from the authenticated caller: {blob}"
        );
        assert_eq!(blob["saved"]["DO_rep_A"], GLOBAL_PARAM_DO_ID);
        assert_eq!(blob["saved"]["Turb"], GLOBAL_PARAM_TURB_ID);
        assert!(blob["saved_at"].as_str().is_some());
    }
    assert_eq!(
        blobs[0].1["run_id"], blobs[1].1["run_id"],
        "the rows of one save share one run"
    );
}

/// Expected behaviour: the `provenance` field no longer exists on the request, and an unknown
/// field is refused rather than dropped, so an old client's self-authored blob cannot slip
/// through as ignored noise.
#[tokio::test]
#[serial]
async fn a_client_authored_blob_is_refused() {
    let (_db, app, token) = setup().await;

    let mut body = tool_save_body(Uuid::new_v4());
    body["provenance"] = json!({ "tool": "forged" });
    let (status, resp) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &body, &token).await;
    assert_eq!(status, 422, "a client-authored blob is refused: {resp}");
    assert!(resp.contains("provenance"), "{resp}");
}

#[tokio::test]
#[serial]
async fn a_save_cannot_claim_a_run_it_did_not_make() {
    let (db, app, token) = setup().await;

    // A run that does not exist.
    let (status, resp) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &tool_save_body(Uuid::new_v4()),
        &token,
    )
    .await;
    assert_eq!(status, 400, "unknown run: {resp}");
    assert!(resp.contains("does not exist"), "{resp}");

    let run_id = mint_run(&db, "doc", run_outputs(), json!([])).await;

    // A value the run did not produce.
    let mut edited = tool_save_body(run_id);
    edited["readings"][0]["value"] = json!(10.5);
    let (status, resp) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &edited, &token).await;
    assert_eq!(status, 400, "an edited value is refused: {resp}");
    assert!(resp.contains("did not compute"), "{resp}");

    // An output the run does not have.
    let mut unknown = tool_save_body(run_id);
    unknown["readings"][0]["output"] = json!("DO_rep_Z");
    let (status, resp) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &unknown, &token).await;
    assert_eq!(status, 400, "an unknown output is refused: {resp}");

    // A reading of a tool save that names no output.
    let mut unnamed = tool_save_body(run_id);
    unnamed["readings"][0]
        .as_object_mut()
        .unwrap()
        .remove("output");
    let (status, resp) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &unnamed, &token).await;
    assert_eq!(status, 400, "an unnamed reading is refused: {resp}");

    // An output claim without a run.
    let mut runless = tool_save_body(run_id);
    runless.as_object_mut().unwrap().remove("tool_run_id");
    let (status, resp) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &runless, &token).await;
    assert_eq!(status, 400, "an output claim without a run is refused: {resp}");
}

/// Expected behaviour: a run that consumed a standard curve produced corrected outputs, so a
/// save referencing it is refused a `standard_curve_id` before any curve admission runs; stamping
/// one would have the correction applied twice (ADR 0003).
#[tokio::test]
#[serial]
async fn a_curve_consuming_run_refuses_a_standard_curve_id() {
    let (db, app, token) = setup().await;
    let run_id = mint_run(
        &db,
        "doc",
        run_outputs(),
        json!([{ "name": "std_curve", "curve": { "slope": 1.05, "intercept": -2.0 } }]),
    )
    .await;

    let mut body = tool_save_body(run_id);
    body["readings"][0]["standard_curve_id"] = json!(Uuid::new_v4());
    let (status, resp) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &body, &token).await;
    assert_eq!(status, 400, "double correction refused: {resp}");
    assert!(resp.contains("twice"), "{resp}");

    // The same save without the curve id is accepted.
    let (status, resp) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &tool_save_body(run_id),
        &token,
    )
    .await;
    assert_eq!(status, 200, "the curveless save lands: {resp}");
}

#[tokio::test]
#[serial]
async fn a_replace_carries_the_new_runs_blob_and_a_plain_save_none() {
    let (db, app, token) = setup().await;

    let first = mint_run(&db, "doc", run_outputs(), json!([])).await;
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &tool_save_body(first),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let second = mint_run(&db, "doc", run_outputs(), json!([])).await;
    let mut replace = tool_save_body(second);
    replace["mode"] = json!("replace");
    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &replace, &token).await;
    assert_eq!(status, 200, "replace lands: {body}");

    for (_, blob) in stored_blobs(&db).await {
        assert_eq!(
            blob["run_id"],
            json!(second),
            "a replace is a new run; the blob follows the numbers written"
        );
    }

    // A plain grab entered by hand carries no blob and must not invent one.
    crate::common::exec(&db, "DELETE FROM readings WHERE measurement_type = 'spot'").await;
    crate::common::exec(
        &db,
        &format!("DELETE FROM samples WHERE site_id = '{SITE1_ID}'"),
    )
    .await;
    let plain = json!({
        "site_id": SITE1_ID,
        "readings": [
            { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 10.0, "time": GRAB_TIME },
            { "parameter_id": GLOBAL_PARAM_TURB_ID, "value": 3.5, "time": GRAB_TIME },
        ],
    });
    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &plain, &token).await;
    assert_eq!(status, 200, "{body}");
    for (_, blob) in stored_blobs(&db).await {
        assert!(
            blob.is_null(),
            "a hand-entered grab has no provenance: {blob}"
        );
    }
}

#[tokio::test]
#[serial]
async fn the_blob_is_not_editable_through_samples_crud() {
    let (db, app, token) = setup().await;

    let run_id = mint_run(&db, "doc", run_outputs(), json!([])).await;
    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &tool_save_body(run_id),
        &token,
    )
    .await;
    assert_eq!(status, 200);

    let sample_id = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT id::text AS id FROM samples \
                 WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}'"
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<String>("", "id")
        .unwrap();

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/samples/{sample_id}"),
        &json!({ "label": "renamed", "provenance": { "tool": "forged" } }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "the label edit itself lands: {body}");

    let (_, blob) = stored_blobs(&db)
        .await
        .into_iter()
        .find(|(pid, _)| pid == GLOBAL_PARAM_DO_ID)
        .expect("sample row exists");
    assert_eq!(
        blob["run_id"],
        json!(run_id),
        "the CRUD update cannot touch the blob: {blob}"
    );
    assert_eq!(blob["tool"], "doc");
}
