//! Scenario: a technician asks why a number changed, from the value itself.
//!
//! Expected behaviour: one read returns everything that happened to the instant, from the records
//! that already hold it, in one row shape and one severity vocabulary, newest first (M141).
//!
//! Run: cargo test --test readings ledger -- --test-threads=1

use sea_orm::DatabaseConnection;
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_DO_ID, SITE1_ID};

const T1: &str = "2025-06-01T08:00:00Z";

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

fn uri(extra: &str) -> String {
    format!(
        "/api/readings/ledger?site_id={SITE1_ID}&parameter_id={GLOBAL_PARAM_DO_ID}&time={T1}{extra}"
    )
}

async fn save_grab(app: &axum::Router, token: &str) {
    let (status, body) = crate::common::post_checked_grab(
        app,
        &json!({
            "site_id": SITE1_ID,
            "readings": [{ "parameter_id": GLOBAL_PARAM_DO_ID, "value": 10.0, "time": T1 }],
        }),
        token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

fn sources(body: &serde_json::Value) -> Vec<String> {
    body["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["source"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
#[serial]
async fn the_ledger_gathers_every_record_that_holds_part_of_the_value_s_history() {
    let (db, app, token) = setup().await;
    save_grab(&app, &token).await;

    // A flag: one curation decision on the instant.
    let (status, body) = crate::common::patch_json_with_token(
        &app,
        "/api/readings/flag",
        &json!({
            "readings": [{
                "site_id": SITE1_ID,
                "parameter_id": GLOBAL_PARAM_DO_ID,
                "time": T1,
                "replicate_index": 0,
                "measurement_type": "spot",
            }],
            "reason": "bubble on the probe",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // A failed job over the slot, and what it said while running.
    let job = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status, error_message, params, \
                                            completed_at) \
             VALUES ('{job}', 'reprocess_site_parameter', 'failed', 'the curve resolved to none', \
                     '{{\"site_id\": \"{SITE1_ID}\", \"parameter_id\": \"{GLOBAL_PARAM_DO_ID}\"}}'::jsonb, \
                     now())"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_job_logs (job_id, seq, level, message) \
             VALUES ('{job}', 1, 'warn', 'skipped the derived step: no definition')"
        ),
    )
    .await;

    let (status, body) = crate::common::get_json_with_token(&app, &uri(""), &token).await;
    assert_eq!(status, 200, "{body}");
    let found = sources(&body);
    for expected in ["decision", "job", "job_log", "change"] {
        assert!(
            found.iter().any(|s| s == expected),
            "the {expected} arm is in the ledger: {body}"
        );
    }

    // Newest first, one shape.
    let entries = body["entries"].as_array().unwrap();
    let times: Vec<&str> = entries.iter().map(|e| e["at"].as_str().unwrap()).collect();
    let mut sorted = times.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(times, sorted, "entries are newest first: {body}");
    // The flag carries its decision, so the history reads its reason and undoes it from here.
    let flag = entries
        .iter()
        .find(|e| e["source"] == "decision")
        .expect("the flag");
    assert_eq!(flag["what"], "flag", "{flag}");
    assert_eq!(flag["decision"]["reason"], "bubble on the probe", "{flag}");
    assert_eq!(flag["decision"]["id"], flag["id"], "{flag}");
    assert_eq!(flag["decision"]["reversible"], true, "{flag}");
    assert!(
        entries
            .iter()
            .filter(|e| e["source"] != "decision")
            .all(|e| e.get("decision").is_none()),
        "only a decision carries one: {body}"
    );
    for e in entries {
        assert!(e["what"].is_string(), "every entry says what happened: {e}");
        assert!(
            ["error", "warning", "info"].contains(&e["severity"].as_str().unwrap()),
            "one severity vocabulary: {e}"
        );
    }
}

#[tokio::test]
#[serial]
async fn failures_are_a_filter_rather_than_a_text_match() {
    let (db, app, token) = setup().await;
    save_grab(&app, &token).await;

    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs (trigger_type, status, error_message, params, \
                                            completed_at) \
             VALUES ('reprocess_site_parameter', 'failed', 'the curve resolved to none', \
                     '{{\"site_id\": \"{SITE1_ID}\", \"parameter_id\": \"{GLOBAL_PARAM_DO_ID}\"}}'::jsonb, \
                     now())"
        ),
    )
    .await;

    let (status, body) =
        crate::common::get_json_with_token(&app, &uri("&severity=error"), &token).await;
    assert_eq!(status, 200, "{body}");
    let entries = body["entries"].as_array().unwrap();
    assert!(!entries.is_empty(), "the failed job is an error: {body}");
    assert!(
        entries.iter().all(|e| e["severity"] == "error"),
        "only failures come back: {body}"
    );
    assert!(
        entries.iter().any(|e| e["source"] == "job"
            && e["what"]
                .as_str()
                .unwrap()
                .contains("the curve resolved to none")),
        "a failed job names its message: {body}"
    );

    let (status, body) =
        crate::common::get_json_with_token(&app, &uri("&severity=loud"), &token).await;
    assert_eq!(status, 400, "an unknown severity is refused: {body}");
}

#[tokio::test]
#[serial]
async fn an_instant_with_no_reading_is_not_found() {
    let (_db, app, token) = setup().await;
    let (status, _) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/readings/ledger?site_id={SITE1_ID}&parameter_id={GLOBAL_PARAM_DO_ID}\
             &time=2019-01-01T00:00:00Z"
        ),
        &token,
    )
    .await;
    assert_eq!(status, 404);
}

#[tokio::test]
#[serial]
async fn arrival_pairing_and_the_receipt_are_rows_of_their_own() {
    let (db, app, token) = setup().await;
    save_grab(&app, &token).await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE data_streams SET paired_at = '2025-06-01T09:08:00Z' \
             WHERE id IN (SELECT stream_id FROM readings WHERE site_id = '{SITE1_ID}' \
                            AND parameter_id = '{GLOBAL_PARAM_DO_ID}' AND time = '{T1}')"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO ingest_receipts \
               (stream_id, at, window_from, window_to, submitted, new_rows, changed, unchanged, \
                retained, rejected_total, rejected, dropped, withdrawn) \
             SELECT stream_id, '2025-06-03T10:44:00Z', '2025-05-01T00:00:00Z', \
                    '2025-06-02T00:00:00Z', 3, 0, 0, 3, 0, 0, '{{}}'::jsonb, 0, 0 \
             FROM readings WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}' \
               AND time = '{T1}' LIMIT 1"
        ),
    )
    .await;

    let (status, body) = crate::common::get_json_with_token(&app, &uri(""), &token).await;
    assert_eq!(status, 200, "{body}");
    let entries = body["entries"].as_array().unwrap();
    let of = |source: &str| {
        entries
            .iter()
            .find(|e| e["source"] == source)
            .unwrap_or_else(|| panic!("a {source} row: {body}"))
    };

    let arrival = of("arrival");
    assert!(arrival["new"]["origin"].is_string(), "{arrival}");
    assert!(arrival["new"]["source_system"].is_string(), "{arrival}");
    assert!(arrival["new"]["source_key"].is_string(), "{arrival}");
    assert_eq!(arrival["new"]["replicates"], json!([0]), "{arrival}");

    let pairing = of("pairing");
    assert_eq!(pairing["at"], "2025-06-01T09:08:00Z", "{pairing}");
    assert_eq!(pairing["new"]["site_id"], SITE1_ID, "{pairing}");
    assert_eq!(
        pairing["new"]["parameter_id"], GLOBAL_PARAM_DO_ID,
        "{pairing}"
    );
    assert!(pairing["new"]["source_key"].is_string(), "{pairing}");

    let receipts: Vec<_> = entries.iter().filter(|e| e["source"] == "ingest").collect();
    assert_eq!(
        receipts.len(),
        1,
        "the receipt and its pass are one row: {body}"
    );
    let receipt = receipts[0];
    assert_eq!(receipt["at"], "2025-06-03T10:44:00Z", "{receipt}");
    for (field, value) in [
        ("submitted", 3),
        ("new", 0),
        ("changed", 0),
        ("unchanged", 3),
        ("withdrawn", 0),
        ("rejected", 0),
    ] {
        assert_eq!(receipt["new"][field], value, "{field}: {receipt}");
    }
    assert_eq!(
        receipt["new"]["window_from"], "2025-05-01T00:00:00Z",
        "{receipt}"
    );
    assert_eq!(
        receipt["new"]["window_to"], "2025-06-02T00:00:00Z",
        "{receipt}"
    );
}
