//! The provenance blob a tool save carries lands on every samples row of the run, is grouped by
//! a shared run_id, follows a replace, and cannot be edited or forged through CRUD.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

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

fn grab_body(provenance: Option<serde_json::Value>) -> serde_json::Value {
    let mut body = json!({
        "site_id": SITE1_ID,
        "readings": [
            { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 10.0, "time": GRAB_TIME },
            { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 12.0, "time": GRAB_TIME },
            { "parameter_id": GLOBAL_PARAM_TURB_ID, "value": 3.5, "time": GRAB_TIME },
        ],
    });
    if let Some(p) = provenance {
        body["provenance"] = p;
    }
    body
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
async fn a_tool_save_stamps_every_sample_row_with_one_run() {
    let (db, app, token) = setup().await;

    let blob = json!({
        "tool": "tss_afdm",
        "tool_version": { "content_hash": "abc" },
        "inputs": { "dried_weight_g": 0.152, "filter_weight_g": 0.1481, "volume_filtered_ml": 500 },
        "outputs": { "TSS_dry_weight_mgL": 7.8 },
        "saved": { "TSS_dry_weight_mgL": GLOBAL_PARAM_DO_ID },
    });
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &grab_body(Some(blob)),
        &token,
    )
    .await;
    assert_eq!(status, 200, "tool save lands: {body}");

    let blobs = stored_blobs(&db).await;
    assert_eq!(blobs.len(), 2, "one samples row per parameter group");
    for (_, blob) in &blobs {
        assert_eq!(blob["tool"], "tss_afdm");
        assert_eq!(blob["inputs"]["volume_filtered_ml"], 500);
        assert!(
            blob["run_id"].as_str().is_some_and(|s| !s.is_empty()),
            "a run_id is minted when the caller sends none: {blob}"
        );
    }
    assert_eq!(
        blobs[0].1["run_id"], blobs[1].1["run_id"],
        "the rows of one save share one run"
    );
}

#[tokio::test]
#[serial]
async fn a_replace_carries_the_new_runs_blob_and_a_plain_save_none() {
    let (db, app, token) = setup().await;

    let first = json!({ "tool": "doc", "inputs": { "replicates": [1.0] }, "run_id": "run-one" });
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &grab_body(Some(first)),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let second = json!({ "tool": "doc", "inputs": { "replicates": [2.0] }, "run_id": "run-two" });
    let mut replace = grab_body(Some(second));
    replace["mode"] = json!("replace");
    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &replace, &token).await;
    assert_eq!(status, 200, "replace lands: {body}");

    for (_, blob) in stored_blobs(&db).await {
        assert_eq!(
            blob["run_id"], "run-two",
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
    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &grab_body(None), &token)
            .await;
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

    let blob = json!({ "tool": "doc", "inputs": {}, "run_id": "run-crud" });
    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &grab_body(Some(blob)),
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
        blob["run_id"], "run-crud",
        "the CRUD update cannot touch the blob: {blob}"
    );
    assert_eq!(blob["tool"], "doc");
}
