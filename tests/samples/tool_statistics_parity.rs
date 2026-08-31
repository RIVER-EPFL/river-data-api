//! The statistic a tool displays and the statistic the database derives from the stored
//! replicates are the same number, which is why only the per-replicate outputs are saved.
//!
//! Scenario: a tool takes a `replicates` input (`DOC`) and emits `_avg` and `_sd` summaries
//! carrying a non-null `aggregate_of`. The portal saves the replicates through
//! `POST /api/grab_samples` and never the summaries; the `samples` trigger recomputes
//! AVG / STDDEV_SAMP / COUNT / MIN / MAX over the unflagged replicates.
//!
//! Expected behaviour: R's `sd()` and Postgres' `STDDEV_SAMP` are both the n-1 sample standard
//! deviation, so the stored statistics equal the tool's own summaries.
//!
//! These tests need the OpenCPU runner on localhost:8006.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::{GLOBAL_PARAM_DO_ID, SITE1_ID};

/// Two independent floating point paths (R and Postgres) reduce the same three f64 values, so a
/// few ulps of disagreement are expected and exact equality is the wrong assertion. At these
/// magnitudes an ulp is around 1e-14, while the convention error this test exists to catch, a
/// population (n) standard deviation where a sample (n-1) one is meant, is a factor of
/// sqrt(2/3) ~= 0.816 on three replicates: for the values below that is a gap of about 10, i.e.
/// ten orders of magnitude larger than this bound.
const TOL: f64 = 1e-9;

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= TOL * b.abs().max(1.0)
}

const GRAB_TIME: &str = "2025-04-02T09:00:00Z";
const OTHER_TIME: &str = "2025-04-02T11:00:00Z";

/// Three distinct, non-zero replicates with a meaningful spread.
const THREE: [f64; 3] = [152.096891818801, 170.668450477067, 255.669204764999];

fn three_replicate_inputs() -> serde_json::Value {
    json!({ "DOC": THREE })
}

/// The middle vial is a gap: position 1 is null, so the vials keep their positions and the
/// summaries reduce over the two measured values.
const GAPPED: [f64; 2] = [150.10958879767, 108.224372683933];

fn gapped_replicate_inputs() -> serde_json::Value {
    json!({ "DOC": [GAPPED[0], null, GAPPED[1]] })
}

struct SampleAggregate {
    mean: Option<f64>,
    stdev: Option<f64>,
    n: i32,
    min_value: Option<f64>,
    max_value: Option<f64>,
}

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

async fn calculate(
    app: &axum::Router,
    tool: &str,
    inputs: serde_json::Value,
    token: &str,
) -> serde_json::Value {
    let (status, json) = crate::common::post_json_parse_with_token(
        app,
        &format!("/api/tools/{tool}/calculate"),
        &inputs,
        token,
    )
    .await;
    assert_eq!(status, 200, "{json}");
    json
}

/// Save a replicate family the way the portal does: the per-replicate values only, one sample per
/// (parameter, time), `replicate_index` in replicate-letter order.
async fn save_replicates(
    app: &axum::Router,
    token: &str,
    time: &str,
    replicates: &[(i16, f64)],
) -> serde_json::Value {
    let readings: Vec<serde_json::Value> = replicates
        .iter()
        .map(|(index, value)| {
            json!({
                "parameter_id": GLOBAL_PARAM_DO_ID,
                "value": value,
                "time": time,
                "replicate_index": index,
            })
        })
        .collect();
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/grab_samples",
        &json!({ "site_id": SITE1_ID, "readings": readings }),
        token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body
}

async fn fetch_aggregate(db: &DatabaseConnection, time: &str) -> SampleAggregate {
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT mean, stdev, n, min_value, max_value FROM samples \
                 WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}' \
                   AND collected_at = '{time}'"
            ),
        ))
        .await
        .expect("query samples")
        .expect("samples row exists");
    SampleAggregate {
        mean: row.try_get("", "mean").unwrap(),
        stdev: row.try_get("", "stdev").unwrap(),
        n: row.try_get("", "n").unwrap(),
        min_value: row.try_get("", "min_value").unwrap(),
        max_value: row.try_get("", "max_value").unwrap(),
    }
}

/// Stored replicates as (replicate_index, raw_value), in index order.
async fn fetch_replicates(db: &DatabaseConnection, time: &str) -> Vec<(i16, f64)> {
    db.query_all(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT replicate_index, raw_value FROM readings \
             WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}' \
               AND time = '{time}' ORDER BY replicate_index"
        ),
    ))
    .await
    .expect("query readings")
    .iter()
    .map(|r| {
        (
            r.try_get::<i16>("", "replicate_index").unwrap(),
            r.try_get::<f64>("", "raw_value").unwrap(),
        )
    })
    .collect()
}

fn number(results: &serde_json::Value, key: &str) -> f64 {
    results[key]
        .as_f64()
        .unwrap_or_else(|| panic!("{key} missing from {results}"))
}

#[tokio::test]
#[serial]
async fn stored_sample_statistics_equal_the_tools_own_summaries() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "stored_sample_statistics_equal_the_tools_own_summaries",
    )
    .await
    {
        return;
    }
    let (db, app, token) = setup().await;

    let run = calculate(&app, "doc", three_replicate_inputs(), &token).await;
    let results = &run["results"];
    let replicates = [(0i16, THREE[0]), (1, THREE[1]), (2, THREE[2])];
    let tool_avg = number(results, "DOC_avg_ppb");
    let tool_sd = number(results, "DOC_sd_ppb");
    assert!(
        tool_sd > 1.0,
        "the inputs must give a meaningful spread, got sd {tool_sd}"
    );

    save_replicates(&app, &token, GRAB_TIME, &replicates).await;

    let agg = fetch_aggregate(&db, GRAB_TIME).await;
    let values: Vec<f64> = replicates.iter().map(|(_, v)| *v).collect();
    let (lo, hi) = (
        values.iter().copied().fold(f64::INFINITY, f64::min),
        values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    );

    assert_eq!(agg.n, 3);
    let (mean, stdev) = (agg.mean.unwrap(), agg.stdev.unwrap());
    assert!(
        close(mean, tool_avg),
        "stored mean {mean} vs tool DOC_avg_ppb {tool_avg}"
    );
    assert!(
        close(stdev, tool_sd),
        "stored stdev {stdev} vs tool DOC_sd_ppb {tool_sd}"
    );
    assert!(close(agg.min_value.unwrap(), lo));
    assert!(close(agg.max_value.unwrap(), hi));

    // The population form of the same spread, which the tolerance must be able to tell apart.
    let population_sd = tool_sd * (2.0_f64 / 3.0).sqrt();
    assert!(
        !close(stdev, population_sd),
        "an n-vs-n-1 mismatch would read as {population_sd} against {stdev}, \
         which a tolerance of {TOL} separates"
    );

    // Only the replicates are stored: no row and no sample carries an aggregate output.
    assert_eq!(fetch_replicates(&db, GRAB_TIME).await.len(), 3);
    let aggregate_rows = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COUNT(*) AS n FROM readings \
                 WHERE site_id = '{SITE1_ID}' AND measurement_type = 'spot' \
                   AND (ABS(raw_value - {tool_avg}) < {TOL} OR ABS(raw_value - {tool_sd}) < {TOL})"
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "n")
        .unwrap();
    assert_eq!(
        aggregate_rows, 0,
        "the _avg and _sd outputs are display-only and are never written as readings"
    );

    let samples = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT COUNT(*) AS n FROM samples WHERE site_id = '{SITE1_ID}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "n")
        .unwrap();
    assert_eq!(
        samples, 1,
        "the summaries claim no parameter slot of their own"
    );
}

/// Expected behaviour: the stored statistics follow the data, not the run that produced it, so
/// excluding a replicate moves them away from the tool's original summaries.
#[tokio::test]
#[serial]
async fn flagging_a_replicate_moves_the_statistics_off_the_original_run() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "flagging_a_replicate_moves_the_statistics_off_the_original_run",
    )
    .await
    {
        return;
    }
    let (db, app, token) = setup().await;

    let run = calculate(&app, "doc", three_replicate_inputs(), &token).await;
    let results = &run["results"];
    let [a, b, c] = THREE;
    let tool_avg = number(results, "DOC_avg_ppb");
    let tool_sd = number(results, "DOC_sd_ppb");

    save_replicates(&app, &token, GRAB_TIME, &[(0, a), (1, b), (2, c)]).await;

    let (status, body) = crate::common::patch_json_with_token(
        &app,
        "/api/readings/flag",
        &json!({
            "readings": [{
                "site_id": SITE1_ID,
                "parameter_id": GLOBAL_PARAM_DO_ID,
                "time": GRAB_TIME,
                "replicate_index": 1,
            }],
            "reason": "replicate excluded",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let agg = fetch_aggregate(&db, GRAB_TIME).await;
    assert_eq!(agg.n, 2);
    let (mean, stdev) = (agg.mean.unwrap(), agg.stdev.unwrap());
    let remaining_mean = (a + c) / 2.0;
    assert!(
        close(mean, remaining_mean),
        "stored mean {mean} recomputed over A and C ({remaining_mean})"
    );
    assert!(close(agg.min_value.unwrap(), a.min(c)));
    assert!(close(agg.max_value.unwrap(), a.max(c)));
    assert!(
        !close(mean, tool_avg),
        "the stored mean has left the run's average {tool_avg}"
    );
    assert!(
        !close(stdev, tool_sd),
        "the stored stdev has left the run's sd {tool_sd}"
    );
}

/// Scenario: the middle vial was not measured, so position 1 is a gap and the measured set is
/// positions 0 and 2.
///
/// Expected behaviour: sent with explicit indices the positions survive the save (0 and 2)
/// and the statistics still match the tool's summaries over the two values. Sent without indices
/// the group numbers from 0, which collapses C onto index 1: the letter is recoverable only from
/// an explicit index.
#[tokio::test]
#[serial]
async fn a_gapped_replicate_set_saves_and_keeps_its_letters_only_when_indices_are_explicit() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_gapped_replicate_set_saves_and_keeps_its_letters_only_when_indices_are_explicit",
    )
    .await
    {
        return;
    }
    let (db, app, token) = setup().await;

    let run = calculate(&app, "doc", gapped_replicate_inputs(), &token).await;
    let results = &run["results"];
    let [a, c] = GAPPED;
    let tool_avg = number(results, "DOC_avg_ppb");
    let tool_sd = number(results, "DOC_sd_ppb");

    save_replicates(&app, &token, GRAB_TIME, &[(0, a), (2, c)]).await;

    assert_eq!(
        fetch_replicates(&db, GRAB_TIME).await,
        vec![(0i16, a), (2i16, c)],
        "letter A is index 0 and letter C is index 2"
    );

    let agg = fetch_aggregate(&db, GRAB_TIME).await;
    assert_eq!(agg.n, 2);
    let (mean, stdev) = (agg.mean.unwrap(), agg.stdev.unwrap());
    assert!(
        close(mean, tool_avg),
        "stored mean {mean} vs tool DOC_avg_ppb {tool_avg}"
    );
    assert!(
        close(stdev, tool_sd),
        "stored stdev {stdev} vs tool DOC_sd_ppb {tool_sd}"
    );

    // The same pair sent without indices: `assign_replicate_indices` numbers the group from 0 in
    // request order, so C lands at index 1 and nothing records that B was skipped.
    let readings: Vec<serde_json::Value> = [a, c]
        .iter()
        .map(|v| json!({ "parameter_id": GLOBAL_PARAM_DO_ID, "value": v, "time": OTHER_TIME }))
        .collect();
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/grab_samples",
        &json!({ "site_id": SITE1_ID, "readings": readings }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        fetch_replicates(&db, OTHER_TIME).await,
        vec![(0i16, a), (1i16, c)],
        "automatic numbering is contiguous, so the letters are lost"
    );

    // The statistics are unaffected either way: the trigger reduces the values, not the indices.
    let automatic = fetch_aggregate(&db, OTHER_TIME).await;
    assert_eq!(automatic.n, 2);
    assert!(close(automatic.mean.unwrap(), tool_avg));
    assert!(close(automatic.stdev.unwrap(), tool_sd));
}

/// Expected behaviour: a mixed or duplicated index set is refused rather than silently renumbered
/// or dropped, which is what makes the explicit-index save above safe to rely on.
#[tokio::test]
#[serial]
async fn a_mixed_or_duplicated_index_set_is_refused() {
    let (_db, app, token) = setup().await;

    let post = async |readings: serde_json::Value| {
        crate::common::post_json_parse_with_token(
            &app,
            "/api/grab_samples",
            &json!({ "site_id": SITE1_ID, "readings": readings }),
            &token,
        )
        .await
    };

    let (mixed_status, mixed) = post(json!([
        { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 1.0, "time": GRAB_TIME, "replicate_index": 0 },
        { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 2.0, "time": GRAB_TIME },
    ]))
    .await;
    assert_eq!(mixed_status, 400, "{mixed}");

    let (dup_status, dup) = post(json!([
        { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 1.0, "time": GRAB_TIME, "replicate_index": 2 },
        { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 2.0, "time": GRAB_TIME, "replicate_index": 2 },
    ]))
    .await;
    assert_eq!(dup_status, 409, "{dup}");
}
