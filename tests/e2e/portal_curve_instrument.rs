//! Standard curves get an instrument, decided in the pairing plan.
//!
//! Scenario: a portal replicates its `standard_curves` table into the API, then registers
//! replicate-family streams whose members are corrected through one of those curves, named per row
//! by a `*_std_curve_id` column. The wizard has to settle which instrument each curve column
//! belongs to before those streams pair.
//!
//! Expected behaviour: a curve column whose stem matches one of the source's own curve labels
//! resolves to that instrument without being asked; one that matches nothing proposes a
//! placeholder and blocks apply until an operator agrees to it; a stream with no curve column and
//! no device serial gets no instrument at all, rather than one minted per stream. With the
//! instrument in place, a reading naming the curve is stored instead of dropped.
//!
//! Run: cargo test --test e2e portal_curve_instrument -- --test-threads=1

use axum::Router;
use serde_json::json;
use serial_test::serial;

use crate::common::e2e;
use crate::common::e2e::count;
use crate::common::keycloak as kc;

const SOURCE: &str = "curvesrc";
const STATION: &str = "CS_A";
const FIXTURE_TIME: &str = "2025-06-02T09:00:00Z";

fn entry_for<'a>(plan: &'a serde_json::Value, stream_id: &str) -> &'a serde_json::Value {
    plan["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("entries array: {plan}"))
        .iter()
        .find(|e| e["stream_id"] == json!(stream_id))
        .unwrap_or_else(|| panic!("entry for stream {stream_id} missing: {plan}"))
}

async fn register_curve(app: &Router, jwt: &str, source_key: &str, label: &str) -> String {
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/standard_curves/register",
        &json!({
            "source_system": SOURCE,
            "source_key": source_key,
            "instrument_label": label,
            "slope": 2.0,
            "intercept": 1.0,
            "name": format!("{label} 2025-01-01"),
        }),
        jwt,
    )
    .await;
    assert_eq!(status, 200, "register curve {label} ({status}): {body}");
    body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("curve id: {body}"))
        .to_string()
}

/// A replicate family the portal corrects through `curve_column`, or a plain single-column stream
/// when `curve_column` is None. Neither carries a device serial, as portal streams do not.
async fn register_stream(
    app: &Router,
    jwt: &str,
    source_key: &str,
    parameter: &str,
    curve_column: Option<&str>,
) -> String {
    let mut payload = json!({
        "source_system": SOURCE,
        "source_key": source_key,
        "source_name": format!("{STATION} - {parameter}"),
        "measurement_type": "spot",
        "metadata": {
            "hierarchy": { "project": "CURVES", "site": STATION, "parameter": parameter },
            "units": "ppb",
        },
    });
    if let Some(column) = curve_column {
        payload["replicates"] = json!({
            "source_columns": [format!("{parameter}_rep_1"), format!("{parameter}_rep_2")],
            "portal_mean_column": format!("{parameter}_avg"),
            "curve_ref_column": column,
            "calc": "calcMean",
        });
    }
    let (status, stream) =
        crate::common::post_json_parse_with_token(app, "/api/streams/register", &payload, jwt).await;
    assert_eq!(status, 200, "register {source_key} ({status}): {stream}");
    e2e::id_of(&stream)
}

/// Apply runs as a tracked job; the counts live on the finished job, not the response.
async fn apply_plan(app: &Router, jwt: &str, plan_id: &str) -> serde_json::Value {
    let (status, res) = crate::common::post_json_parse_with_token(
        app,
        &format!("/api/sync/pairing-plans/{plan_id}/apply"),
        &json!({}),
        jwt,
    )
    .await;
    assert_eq!(status, 200, "apply ({status}): {res}");
    let job_id = res["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("apply returns a job_id: {res}"));
    assert_eq!(
        e2e::poll_job(app, jwt, job_id, 30).await,
        "completed",
        "apply job completes",
    );
    let (_, job) =
        crate::common::get_json_with_token(app, &format!("/api/reprocessing_jobs/{job_id}"), jwt)
            .await;
    job["detail"]["counts"].clone()
}

async fn create_plan(app: &Router, jwt: &str) -> serde_json::Value {
    let (status, plan) = crate::common::post_json_parse_with_token(
        app,
        "/api/sync/pairing-plans",
        &json!({ "source_system": SOURCE }),
        jwt,
    )
    .await;
    assert_eq!(status, 200, "create plan ({status}): {plan}");
    plan
}

#[tokio::test]
#[serial]
async fn curve_columns_resolve_to_instruments_before_their_streams_pair() {
    if !kc::require_keycloak_or_skip("portal_curve_instrument").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let curve_id = register_curve(&app, &admin, "standard_curves:1", "DOC corr").await;
    let sensors_after_curve = count(&db, "SELECT COUNT(*) FROM sensors").await;
    assert_eq!(
        sensors_after_curve, 1,
        "registering a curve creates exactly its lab instrument",
    );

    let doc = register_stream(&app, &admin, "doc", "DOC", Some("doc_std_curve_id")).await;
    let xyz = register_stream(&app, &admin, "xyz", "XYZ", Some("xyz_std_curve_id")).await;
    let plain = register_stream(&app, &admin, "plain", "Depth", None).await;

    let plan = create_plan(&app, &admin).await;
    let plan_id = e2e::id_of(&plan);

    let doc_instrument = &entry_for(&plan, &doc)["instrument"];
    assert_eq!(
        doc_instrument["resolved_by"], "curve_label",
        "doc_std_curve_id matches the source's own 'DOC corr' label: {doc_instrument}",
    );
    assert_eq!(
        doc_instrument["create"], false,
        "a matched instrument is not created again: {doc_instrument}",
    );
    assert_eq!(
        doc_instrument["stamps_readings"], true,
        "the family's own calculation names the curve, so each reading stores it: {doc_instrument}",
    );
    assert_eq!(
        doc_instrument["curves"]
            .as_array()
            .map(|c| c.len())
            .unwrap_or_default(),
        1,
        "the instrument's curves travel with the entry: {doc_instrument}",
    );

    let xyz_instrument = &entry_for(&plan, &xyz)["instrument"];
    assert_eq!(
        xyz_instrument["resolved_by"], "placeholder",
        "nothing matches xyz_std_curve_id: {xyz_instrument}",
    );
    assert_eq!(xyz_instrument["create"], true, "{xyz_instrument}");
    assert_eq!(
        xyz_instrument["confirmed"], false,
        "a proposal is not an agreement: {xyz_instrument}",
    );

    assert!(
        entry_for(&plan, &plain)["instrument"].is_null(),
        "a stream with no curve column needs no instrument: {plan}",
    );
    assert_eq!(
        plan["summary"]["instruments_to_create"], 1,
        "counted by identity, not by stream: {}",
        plan["summary"],
    );

    let (status, refused) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}/apply"),
        &json!({}),
        &admin,
    )
    .await;
    assert_eq!(
        status, 400,
        "apply is refused while an instrument is unconfirmed: {refused}",
    );
    assert!(
        refused.contains("xyz"),
        "the refusal names the stream that needs a decision: {refused}",
    );

    let (status, patched) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &json!({ "updates": [{
            "stream_id": xyz,
            "instrument_name": "XYZ lab (curvesrc portal)",
            "instrument_confirmed": true,
        }] }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "confirm the proposal ({status}): {patched}");

    let counts = apply_plan(&app, &admin, &plan_id).await;
    assert_eq!(
        counts["instruments_created"], 1,
        "one instrument for the confirmed column, not one per stream: {counts}",
    );
    assert_eq!(counts["streams_paired"], 3, "{counts}");

    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM sensors").await,
        2,
        "the curve's instrument plus the one confirmed in the review, and nothing else",
    );
    assert_eq!(
        count(
            &db,
            &format!("SELECT COUNT(*) FROM data_streams WHERE id = '{plain}' AND sensor_id IS NULL")
        )
        .await,
        1,
        "a portal stream with no device serial and no curve column stays unattributed",
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM data_streams WHERE id IN ('{doc}', '{xyz}') \
                 AND sensor_id IS NOT NULL"
            )
        )
        .await,
        2,
        "both curve streams carry the instrument their curves belong to",
    );

    let (status, ingested) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &json!({
            "stream_id": doc,
            "readings": [{
                "time": FIXTURE_TIME,
                "replicate_index": 0,
                "raw_value": 10.0,
                "standard_curve_id": curve_id,
            }],
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "ingest ({status}): {ingested}");
    assert_eq!(
        ingested["inserted"], 1,
        "a reading naming a curve lands once its stream names that curve's instrument: {ingested}",
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM readings WHERE stream_id = '{doc}' \
                 AND standard_curve_id = '{curve_id}'"
            )
        )
        .await,
        1,
        "the curve reference is stored, not dropped as an inadmissible claim",
    );

    // The same claim on the stream that names no instrument, which is the state every curve stream
    // was in before the plan settled one. Nothing errors: the reading is counted out and gone,
    // which is why an unresolved curve column has to block the apply rather than warn about it.
    let (status, dropped) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &json!({
            "stream_id": plain,
            "readings": [{
                "time": FIXTURE_TIME,
                "raw_value": 10.0,
                "standard_curve_id": curve_id,
            }],
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "ingest ({status}): {dropped}");
    assert_eq!(
        dropped["inserted"], 0,
        "a curve claim from a stream naming no instrument stores nothing: {dropped}",
    );
    assert_eq!(dropped["skipped"], 1, "{dropped}");
    assert!(
        dropped["skipped_reasons"]
            .as_array()
            .is_some_and(|r| r
                .iter()
                .any(|s| s.as_str().is_some_and(|s| s.contains("standard_curve_id")))),
        "and says so: {dropped}",
    );
}
