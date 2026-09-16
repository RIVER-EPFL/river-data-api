//! Standard curves get an instrument, decided in the pairing plan.
//!
//! Scenario: a portal replicates its `standard_curves` table into the API, then registers
//! replicate-family streams whose members are corrected through one of those curves, named per row
//! by a `*_std_curve_id` column. The wizard has to settle which instrument each curve column
//! belongs to before those streams pair.
//!
//! Expected behaviour: the curves are held, not stored, because the portal names no instrument for
//! them (Q195); each curve column proposes an instrument an operator confirms; a held curve is
//! attached to one of the plan's instruments and the apply creates it there; a stream with no
//! curve column is proposed the source's instrument for its parameter. With the instrument and the
//! curve in place, a reading naming the curve is stored instead of dropped.
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

/// Register a portal curve, which the API holds for a plan.
async fn register_curve(app: &Router, jwt: &str, source_key: &str, label: &str) {
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
    assert_eq!(
        body["proposed"], true,
        "held until a plan attaches it: {body}"
    );
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
        crate::common::post_json_parse_with_token(app, "/api/streams/register", &payload, jwt)
            .await;
    assert_eq!(status, 200, "register {source_key} ({status}): {stream}");
    e2e::id_of(&stream)
}

/// Apply runs as a tracked job; the counts live on the finished job, not the response.
async fn apply_plan(app: &Router, jwt: &str, plan_id: &str) -> serde_json::Value {
    // The review gate wants every row ticked; this story is about the curves and instruments the
    // apply binds.
    crate::common::plans::acknowledge_plan(app, jwt, plan_id).await;
    let (status, res) =
        crate::common::post_plan_action_parse_with_token(app, &plan_id.to_string(), "apply", jwt)
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
    if !crate::common::profile::Service::Keycloak
        .require("portal_curve_instrument")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    register_curve(&app, &admin, "standard_curves:1", "DOC corr").await;
    // The harness seeds one instrument of its own, so the question is what the registration added.
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM sensors WHERE id <> '{}'",
                crate::common::FIXTURE_SENSOR_ID
            ),
        )
        .await,
        0,
        "registering a curve creates no instrument",
    );

    let doc = register_stream(&app, &admin, "doc", "DOC", Some("doc_std_curve_id")).await;
    let xyz = register_stream(&app, &admin, "xyz", "XYZ", Some("xyz_std_curve_id")).await;
    let plain = register_stream(&app, &admin, "plain", "Depth", None).await;

    let plan = create_plan(&app, &admin).await;
    let plan_id = e2e::id_of(&plan);

    for (stream, column) in [(&doc, "doc_std_curve_id"), (&xyz, "xyz_std_curve_id")] {
        let instrument = &entry_for(&plan, stream)["instrument"];
        assert_eq!(
            instrument["resolved_by"], "placeholder",
            "no instrument exists for {column} to match: {instrument}",
        );
        assert_eq!(instrument["create"], true, "{instrument}");
        assert_eq!(
            instrument["confirmed"], false,
            "a proposal is not an agreement: {instrument}",
        );
        assert_eq!(
            instrument["stamps_readings"], true,
            "the family's own calculation names the curve, so each reading stores it: {instrument}",
        );
    }

    // Registration mints nothing (M172), so a stream with no curve column is proposed one under
    // the source's own parameter key, unconfirmed like every suggestion (Q195).
    let plain_instrument = &entry_for(&plan, &plain)["instrument"];
    assert_eq!(
        plain_instrument["resolved_by"], "parameter",
        "the source's instrument for the parameter it carries: {plain_instrument}",
    );
    assert_eq!(plain_instrument["create"], true, "{plain_instrument}");
    assert_eq!(
        plain_instrument["confirmed"], false,
        "a suggestion waits for a person even when nothing else carries the name: {plain_instrument}",
    );
    assert_eq!(
        plain_instrument["stamps_readings"], false,
        "no curve column, so no curve is stored per reading: {plain_instrument}",
    );
    assert_eq!(
        plan["summary"]["instruments_to_create"], 3,
        "counted by identity, not by stream: {}",
        plan["summary"],
    );

    let (status, refused) =
        crate::common::post_plan_action_with_token(&app, &plan_id.to_string(), "apply", &admin)
            .await;
    assert_eq!(
        status, 400,
        "apply is refused while an instrument is unconfirmed: {refused}",
    );
    assert!(
        refused.contains("xyz"),
        "the refusal names the stream that needs a decision: {refused}",
    );

    for (stream, name) in [
        (&doc, "DOC lab (curvesrc portal)"),
        (&xyz, "XYZ lab (curvesrc portal)"),
    ] {
        let (status, patched) = crate::common::patch_plan_with_token(
            &app,
            &plan_id.to_string(),
            &json!({ "updates": [{
                "stream_id": stream,
                "instrument_name": name,
                "instrument_confirmed": true,
            }] }),
            &admin,
        )
        .await;
        assert_eq!(status, 200, "confirm the proposal ({status}): {patched}");
    }

    // The held curve blocks the apply until it is attached to the instrument its column names.
    let (status, view) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}/instruments"),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "instruments view ({status}): {view}");
    let held = view["held_curves"].as_array().expect("held curves");
    assert_eq!(held.len(), 1, "{view}");
    let (_, plan) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &admin,
    )
    .await;
    let (status, attached) = crate::common::patch_plan_with_token(
        &app,
        &plan_id.to_string(),
        &json!({ "held_curves": [{
            "proposal_id": held[0]["id"],
            "instrument_source_key": entry_for(&plan, &doc)["instrument"]["source_key"],
        }] }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "attach the held curve ({status}): {attached}");

    let before_apply = count(&db, "SELECT COUNT(*) FROM sensors").await;
    let counts = apply_plan(&app, &admin, &plan_id).await;
    assert_eq!(
        counts["instruments_created"], 3,
        "one per confirmed curve column and one for the parameter, not one per stream: {counts}",
    );
    assert_eq!(counts["streams_paired"], 3, "{counts}");

    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM sensors").await,
        before_apply + 3,
        "the apply creates the three the plan named, and nothing else",
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM data_streams WHERE id = '{plain}' AND sensor_id IS NOT NULL"
            )
        )
        .await,
        1,
        "a stream with no curve column reaches its slot with an instrument all the same: no \
         measurement without one",
    );
    let curve_id = e2e::scalar(
        &db,
        &format!(
            "SELECT c.id::text FROM standard_curves c JOIN data_streams ds ON ds.sensor_id = c.sensor_id \
             WHERE ds.id = '{doc}' AND c.source_system = '{SOURCE}' \
               AND c.source_key = 'standard_curves:1'"
        ),
    )
    .await;

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
    // was in before the plan settled one. The reading is stored uncorrected, the claim is stripped
    // into a review hold, and nothing is lost. The unresolved curve column still blocks the plan
    // apply: until an instrument is settled, no claim on the stream can ever verify.
    let (status, stripped) = crate::common::post_json_parse_with_token(
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
    assert_eq!(status, 200, "ingest ({status}): {stripped}");
    assert_eq!(
        stripped["inserted"], 1,
        "the reading is stored; only the unverifiable claim is refused: {stripped}",
    );
    assert_eq!(stripped["skipped"], 0, "{stripped}");
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM readings WHERE stream_id = '{plain}' \
                 AND standard_curve_id IS NULL AND calibrated_value IS NULL"
            )
        )
        .await,
        1,
        "stored uncorrected with no stamped curve",
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM replicate_audit_holds WHERE stream_id = '{plain}' \
                 AND kind = 'curve_claim_stripped' AND status = 'pending'"
            )
        )
        .await,
        1,
        "the stripped claim is a review-queue hold",
    );
}

/// Scenario: a portal that corrects a value upstream names no curve per reading, so nothing
/// connects those streams to the fluorometer that measured them. Every stream still carries an
/// instrument, because registration mints one per (source, parameter); it is just not the right
/// one, and no inference can reach the right one.
///
/// Expected behaviour: the plan reports what it resolved and by what, an operator attaches the
/// instrument by parameter (settling every station at once, which is how two chla columns reach one
/// fluorometer), and the apply stores it on the streams without stamping a curve on any reading.
#[tokio::test]
#[serial]
async fn an_instrument_is_attached_to_streams_whose_source_names_no_curve() {
    if !crate::common::profile::Service::Keycloak
        .require("portal_curve_instrument")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let (_, instrument_id) = crate::common::store_source_curve(
        &db,
        SOURCE,
        "Chla fluorometer",
        "standard_curves:1",
        "Chla fluorometer 2025-01-01",
        2.0,
        1.0,
    )
    .await;
    let instrument_id = instrument_id.to_string();
    let acid = register_stream(&app, &admin, "chla_acid", "chla_acid", None).await;
    let noacid = register_stream(&app, &admin, "chla_noacid", "chla_noacid", None).await;
    let bix = register_stream(&app, &admin, "bix", "BIX", None).await;

    let plan = create_plan(&app, &admin).await;
    let plan_id = e2e::id_of(&plan);

    let (status, view) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}/instruments"),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "instruments view ({status}): {view}");
    let groups = view["groups"].as_array().expect("groups array");
    assert_eq!(
        groups.len(),
        3,
        "each stream's own proposal is reported, one per parameter: {view}",
    );
    assert!(
        groups
            .iter()
            .all(|g| g["resolved_by"] == "parameter" && g["instrument_id"] != instrument_id),
        "none of them is the fluorometer, which is the gap the operator closes below: {view}",
    );
    assert!(
        view["unassigned"].as_array().is_some_and(Vec::is_empty),
        "no measurement without an instrument: nothing is left unattributed: {view}",
    );

    for stream in [&acid, &noacid] {
        let (status, patched) = crate::common::patch_plan_with_token(
            &app,
            &plan_id.to_string(),
            &json!({ "updates": [{ "stream_id": stream, "instrument_id": instrument_id }] }),
            &admin,
        )
        .await;
        assert_eq!(status, 200, "attach ({status}): {patched}");
    }

    let (_, plan) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &admin,
    )
    .await;
    let attached = &entry_for(&plan, &acid)["instrument"];
    assert_eq!(attached["resolved_by"], "manual", "{attached}");
    assert_eq!(
        attached["stamps_readings"], false,
        "the value already carries the correction, so nothing is applied a second time: {attached}",
    );
    assert_eq!(
        attached["curves"].as_array().map(Vec::len),
        Some(1),
        "the instrument's curves travel with the entry it was attached to: {attached}",
    );

    // A parameter no registered instrument covers gets one named here rather than left without.
    let (status, patched) = crate::common::patch_plan_with_token(
        &app,
        &plan_id.to_string(),
        &json!({ "updates": [{
            "stream_id": bix,
            "instrument_name": "BIX instrument (from curvesrc sync)",
            "instrument_confirmed": true,
        }] }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "name a new instrument ({status}): {patched}");

    apply_plan(&app, &admin, &plan_id).await;
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM data_streams \
                 WHERE id IN ('{acid}', '{noacid}') AND sensor_id = '{instrument_id}'"
            )
        )
        .await,
        2,
        "both columns land on the one fluorometer",
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM data_streams ds JOIN sensors s ON s.id = ds.sensor_id \
                 WHERE ds.id = '{bix}' AND s.name = 'BIX instrument (from curvesrc sync)' \
                 AND s.is_lab_instrument"
            )
        )
        .await,
        1,
        "the named instrument is minted and carried onto its stream",
    );
}
