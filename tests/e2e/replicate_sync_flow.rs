//! The whole replicate-sync story from a fresh database, driven through HTTP: a portal's standard
//! curve and replicate family are registered, replicate batches ingest with curve correction and
//! a matching statistics audit, the reconciliation job migrates the legacy avg stream's slot onto
//! the family behind two verifications, an audit mismatch is admitted and reviewed, and the
//! delete job finally retires the avg stream without moving a served value.
//!
//! Run: cargo test --test e2e replicate_sync_flow -- --test-threads=1

use crate::common::e2e;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

const T1: &str = "2025-06-01T08:00:00Z";
const T2: &str = "2025-06-01T09:00:00Z";
const T3: &str = "2025-06-01T10:00:00Z";
const T4: &str = "2025-06-01T11:00:00Z";

/// Replicate raw counts per instant; the curve (slope 2, intercept 1) turns them into the
/// corrected values whose mean the legacy avg stream served.
const GROUPS: [(&str, [f64; 3], f64, f64); 3] = [
    (T1, [10.0, 20.0, 30.0], 41.0, 20.0),
    (T2, [40.0, 50.0, 60.0], 101.0, 20.0),
    (T3, [5.0, 8.0, 11.0], 17.0, 6.0),
];

async fn scalar_f64(db: &DatabaseConnection, sql: &str) -> f64 {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<f64>("", "v")
    .unwrap()
}

fn family_group(time: &str, raws: &[f64; 3], curve: &str) -> Vec<serde_json::Value> {
    raws.iter()
        .enumerate()
        .map(|(i, v)| {
            json!({"time": time, "raw_value": v, "replicate_index": i,
                   "standard_curve_id": curve})
        })
        .collect()
}

fn sorted(mut values: Vec<f64>) -> Vec<f64> {
    values.sort_by(f64::total_cmp);
    values
}

#[tokio::test]
#[serial]
async fn replicate_sync_full_flow() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (sync_token, _service_id) = crate::common::seed_sync_session_token(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let project = e2e::create_project(&app, &token, "Replicate P", "repl-p", false).await;
    let site = e2e::create_site(&app, &token, &project, "DGT", "dgt").await;
    let param = e2e::create_parameter(&app, &token, "doclab", "DOC", "ppb").await;
    let sp = e2e::assign_site_parameter_minimal(&app, &token, &site, &param).await;

    let register_curve = |slope: f64| {
        json!({
            "source_system": "cnet",
            "source_key": "standard_curves:17",
            "instrument_label": "DOC corr",
            "slope": slope,
            "intercept": 1.0,
        })
    };
    let (status, curve_resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/standard_curves/register",
        &register_curve(2.0),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "register curve ({status}): {curve_resp}");
    let curve = curve_resp["id"].as_str().unwrap().to_string();
    let lab_sensor = curve_resp["sensor_id"].as_str().unwrap().to_string();

    let family_registration = |replicates: serde_json::Value, mt: Option<&str>| {
        let mut body = json!({
            "source_system": "cnet",
            "source_key": "DGT:DOC_avg_ppb:reps",
            "sensor_id": lab_sensor,
            "replicates": replicates,
        });
        if let Some(mt) = mt {
            body["measurement_type"] = json!(mt);
        }
        body
    };
    let full_spec = json!({
        "source_columns": ["DOC_1_ppb", "DOC_2_ppb", "DOC_3_ppb"],
        "portal_mean_column": "DOC_avg_ppb",
        "portal_sd_column": "DOC_sd_ppb",
        "curve_ref_column": "doc_std_curve_id",
        "calc": "calcDOCavg",
    });

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/streams/register",
        &family_registration(full_spec.clone(), None),
        &sync_token,
    )
    .await;
    assert_eq!(
        status, 400,
        "a replicate spec without spot is refused: {body}"
    );
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/streams/register",
        &family_registration(json!({"source_columns": ["DOC_1_ppb"]}), Some("spot")),
        &sync_token,
    )
    .await;
    assert_eq!(status, 400, "a single-member spec is refused: {body}");

    let (status, family_stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &family_registration(full_spec, Some("spot")),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "register family ({status}): {family_stream}");
    let family_stream = e2e::id_of(&family_stream);
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM data_streams WHERE id = '{family_stream}' \
                 AND metadata->'replicates'->'source_columns' IS NOT NULL"
            ),
        )
        .await,
        1,
        "the spec is persisted under metadata.replicates"
    );

    let (status, old_stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({"source_system": "cnet", "source_key": "DGT:DOC_avg_ppb",
                "measurement_type": "spot"}),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "register avg stream ({status}): {old_stream}");
    let old_stream = e2e::id_of(&old_stream);
    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{old_stream}/pair"),
        &json!({"site_parameter_id": sp}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "pair avg stream ({status}): {body}");

    let avg_readings: Vec<serde_json::Value> = GROUPS
        .iter()
        .map(|(time, _raws, mean, _sd)| json!({"time": time, "raw_value": mean}))
        .collect();
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &json!({"stream_id": old_stream, "readings": avg_readings}),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "avg ingest ({status}): {body}");
    assert_eq!(body["inserted"], 3);

    let mut readings = Vec::new();
    let mut audit = Vec::new();
    for (time, raws, mean, sd) in &GROUPS {
        readings.extend(family_group(time, raws, &curve));
        audit.push(json!({"time": time, "expected_mean": mean, "expected_sd": sd}));
    }
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &json!({"stream_id": family_stream, "readings": readings, "audit": audit}),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "family ingest ({status}): {body}");
    assert_eq!(body["inserted"], 9);
    assert_eq!(body["held"], 0, "the portal's statistics agree: {body}");
    assert_eq!(body["paired"], false);

    assert_eq!(
        e2e::count(&db, "SELECT COUNT(*) FROM replicate_audit_holds").await,
        0
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT COUNT(*) FROM samples WHERE site_id = '{site}'"),
        )
        .await,
        0,
        "unpaired: no samples yet"
    );

    let (status, candidates) = crate::common::get_json_with_token(
        &app,
        "/api/sync/replicate_reconciliation/candidates?source_system=cnet",
        &token,
    )
    .await;
    assert_eq!(status, 200, "candidates: {candidates}");
    assert_eq!(candidates["families"][0]["ready"], true, "{candidates}");
    assert_eq!(candidates["families"][0]["missing_instants"], 0);
    assert_eq!(candidates["families"][0]["migrated"], false);

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/replicate_reconciliation",
        &json!({"source_system": "cnet"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "start reconciliation ({status}): {body}");
    let job_id = body["job_id"].as_str().unwrap().to_string();
    assert_eq!(
        e2e::poll_job(&app, &token, &job_id, 30).await,
        "completed",
        "migrate job"
    );

    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM data_streams WHERE id = '{family_stream}' \
                 AND site_parameter_id = '{sp}'"
            ),
        )
        .await,
        1,
        "the family took the avg stream's slot"
    );
    for (time, _raws, mean, sd) in &GROUPS {
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "SELECT mean, stdev, n::bigint AS n FROM samples \
                     WHERE site_id = '{site}' AND parameter_id = '{param}' \
                     AND collected_at = '{time}'"
                ),
            ))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("sample at {time} exists"));
        assert_eq!(row.try_get::<i64>("", "n").unwrap(), 3);
        let got_mean: f64 = row.try_get("", "mean").unwrap();
        let got_sd: f64 = row.try_get("", "stdev").unwrap();
        assert!(
            (got_mean - mean).abs() < 1e-9,
            "sample mean at {time}: {got_mean} vs {mean}"
        );
        assert!(
            (got_sd - sd).abs() < 1e-9,
            "sample stdev at {time}: {got_sd} vs {sd}"
        );
    }

    let series_uri = format!(
        "/api/sites/{site}/readings?start=2025-06-01T00:00:00Z&end=2025-06-02T00:00:00Z\
         &parameter_ids={param}&measurement_type=spot&include_sample_stats=true"
    );
    let (status, series) = crate::common::get_json_with_token(&app, &series_uri, &token).await;
    assert_eq!(status, 200, "series: {series}");
    let served = e2e::values_for(&series, &param);
    assert_eq!(served.len(), 3, "one point per instant: {series}");
    for ((_, _, mean, _), got) in GROUPS.iter().zip(&served) {
        assert!(
            (got - mean).abs() < 1e-9,
            "the migrated slot serves the old avg values 1:1: {served:?}"
        );
    }

    // A follow-up sync cycle whose portal mean disagrees with what the replicates produce.
    let mismatch_batch = family_group(T4, &[12.0, 24.0, 36.0], &curve);
    let mismatch_audit = json!([{"time": T4, "expected_mean": 50.0, "expected_sd": 24.0}]);
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &json!({"stream_id": family_stream, "readings": mismatch_batch.clone(),
                "audit": mismatch_audit}),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "mismatch ingest ({status}): {body}");
    assert_eq!(
        body["inserted"], 3,
        "the disagreeing group is admitted for review: {body}"
    );
    assert_eq!(body["held"], 0);
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM readings WHERE stream_id = '{family_stream}' \
                 AND time = '{T4}'"
            ),
        )
        .await,
        3
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM data_streams WHERE id = '{family_stream}' \
                 AND last_data_time = '{T4}'"
            ),
        )
        .await,
        1,
        "the cursor advances past the reviewed group"
    );

    let (status, holds) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sync/replicate_audit_holds?stream_id={family_stream}"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "holds: {holds}");
    assert_eq!(holds["pending"], 1);
    let hold_id = holds["holds"][0]["id"].as_str().unwrap().to_string();

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sync/replicate_audit_holds/{hold_id}/acknowledge"),
        &json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "acknowledge ({status}): {body}");

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &json!({"stream_id": family_stream, "readings": mismatch_batch,
                "audit": [{"time": T4, "expected_mean": 50.0, "expected_sd": 24.0}]}),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "re-send ({status}): {body}");
    assert_eq!(
        body["inserted"], 0,
        "the re-send is duplicate-skipped: {body}"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM replicate_audit_holds WHERE id = '{hold_id}' \
                 AND status = 'acknowledged'"
            ),
        )
        .await,
        1,
        "the decision stands against re-detection"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM replicate_audit_holds \
                 WHERE stream_id = '{family_stream}'"
            ),
        )
        .await,
        1,
        "no second hold opens for the decided group"
    );
    let t4_mean = scalar_f64(
        &db,
        &format!(
            "SELECT mean AS v FROM samples WHERE site_id = '{site}' \
             AND parameter_id = '{param}' AND collected_at = '{T4}'"
        ),
    )
    .await;
    assert!(
        (t4_mean - 49.0).abs() < 1e-9,
        "the recomputed mean (25+49+73)/3 is what is served, not the portal's 50: {t4_mean}"
    );

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/replicate_reconciliation/delete",
        &json!({"source_system": "cnet"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "start delete ({status}): {body}");
    let job_id = body["job_id"].as_str().unwrap().to_string();
    assert_eq!(
        e2e::poll_job(&app, &token, &job_id, 30).await,
        "completed",
        "delete job"
    );

    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT COUNT(*) FROM data_streams WHERE id = '{old_stream}'"),
        )
        .await,
        0,
        "the legacy avg stream is retired"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT COUNT(*) FROM readings WHERE stream_id = '{old_stream}'"),
        )
        .await,
        0
    );

    let (status, series) = crate::common::get_json_with_token(&app, &series_uri, &token).await;
    assert_eq!(status, 200, "series after delete: {series}");
    let served = sorted(e2e::values_for(&series, &param));
    let expected = sorted(vec![41.0, 101.0, 17.0, 49.0]);
    assert_eq!(served.len(), 4, "one point per collection event: {series}");
    for (got, want) in served.iter().zip(&expected) {
        assert!(
            (got - want).abs() < 1e-9,
            "served values unchanged by the deletion: {served:?} vs {expected:?}"
        );
    }
    let sample_stats = series["parameters"][0]["samples"].as_array().unwrap();
    assert_eq!(
        sample_stats.iter().filter(|s| s["n"] == 3).count(),
        4,
        "every point is a replicate group of three: {series}"
    );
}
