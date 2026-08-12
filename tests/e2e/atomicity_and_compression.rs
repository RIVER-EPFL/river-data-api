//! Multi-statement mutations driven over history old enough to have been compressed.
//!
//! Scenario: an operator corrects the record months after the fact, on chunks TimescaleDB has
//! since compressed (the production policy compresses `readings` after 30 days, segmented by
//! `stream_id`).
//! Expected behaviour: every one of these operations applies in full or leaves nothing behind, and
//! none of them fails merely because the rows it must rewrite are compressed. Pairing, adopt, swap,
//! reprocess and the sync writers lift the decompression cap for exactly this reason; the paths
//! covered here run the same kind of statement without it.
//!
//! Each test builds the app on a pool that caps decompression at [`CAP`] tuples per DML statement.
//! That cap is what makes the guarded/unguarded distinction observable at all: the server default
//! is 100000 tuples, so a small compressed fixture mutates fine either way. `assert_cap_bites` runs
//! immediately before every operation under test and proves the fixture really is compressed and
//! the cap really is in force, so a green run cannot be a vacuous one.
//!
//! Fixture days are weeks apart so each lands in its own chunk, and every one is in the past: the
//! aggregate refresh window is `[since, NOW()]`, so a future-dated fixture is never materialised.
//!
//! These run as real Keycloak users, each step as the level that owns it: provisioning and stream
//! pairing are Administrator work, instrument and catalog surgery is MANAGER work, ingestion and
//! flagging are RIVER work. They self-skip when Keycloak is unreachable unless `REQUIRE_KEYCLOAK`
//! is set.

use axum::Router;
use chrono::{DateTime, Duration, Utc};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::{Value, json};
use serial_test::serial;

use crate::common::compression::{compress_readings_range, connect_with_decompression_cap};
use crate::common::e2e;
use crate::common::keycloak as kc;
use crate::common::tracks;

/// Tuples one DML statement may decompress on this pool before Postgres refuses it.
const CAP: u32 = 20;

/// Rows in a compressed fixture. Above `CAP`, so one unguarded statement over them is refused.
const COMPRESSED_ROWS: usize = 40;

/// App bound to a pool whose every session carries the decompression cap, plus that pool for the
/// harness's own probes. Verification queries use the uncapped connection from `setup_test_db`.
async fn capped_app() -> (Router, DatabaseConnection) {
    let capped = connect_with_decompression_cap(CAP).await;
    let app = kc::build_test_app_with_keycloak(capped.clone()).await;
    (app, capped)
}

/// Manager and river JWTs, both granted the project under test.
async fn operator_jwts(db: &DatabaseConnection, project_id: &str) -> (String, String) {
    kc::ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    kc::grant_project(db, &kc::keycloak_user_id("manager1").await, project_id).await;
    kc::grant_project(db, &kc::keycloak_user_id("river1").await, project_id).await;
    (
        kc::get_keycloak_jwt("manager1", "manager1").await,
        kc::get_keycloak_jwt("river1", "river1").await,
    )
}

fn day(date: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&format!("{date}T00:00:00Z"))
        .unwrap_or_else(|e| panic!("{date} is not a date: {e}"))
        .with_timezone(&Utc)
}

/// `count` timestamps one minute apart from midnight on `date`.
fn minutes(date: &str, count: usize) -> Vec<DateTime<Utc>> {
    let start = day(date);
    (0..count)
        .map(|i| start + Duration::minutes(i as i64))
        .collect()
}

/// `count` timestamps 15 seconds apart from midnight on `date`, so 200 of them share one hourly
/// bucket.
fn quarter_minutes(date: &str, count: usize) -> Vec<DateTime<Utc>> {
    let start = day(date);
    (0..count)
        .map(|i| start + Duration::seconds(15 * i as i64))
        .collect()
}

/// Compress the `readings` chunk holding `date`, the state the 30-day policy reaches on its own.
async fn compress_day(db: &DatabaseConnection, date: &str) {
    let start = day(date);
    let compressed =
        compress_readings_range(db, start - Duration::hours(1), start + Duration::days(1)).await;
    assert!(
        compressed >= 1,
        "the {date} readings must sit in a chunk this step compresses, else the operation under \
         test never meets a compressed row"
    );
}

/// The `status_events` counterpart of [`compress_day`]; that hypertable carries the same
/// `compress_segmentby = 'stream_id'` setting and a 90-day policy.
async fn compress_status_events_day(db: &DatabaseConnection, date: &str) {
    let start = day(date);
    let chunks = db
        .query_all(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT format('%I.%I', chunk_schema, chunk_name) AS chunk \
             FROM timescaledb_information.chunks \
             WHERE hypertable_name = 'status_events' AND NOT is_compressed \
               AND range_end > $1 AND range_start <= $2",
            [
                (start - Duration::hours(1)).into(),
                (start + Duration::days(1)).into(),
            ],
        ))
        .await
        .expect("status_events chunk lookup failed");
    assert!(
        !chunks.is_empty(),
        "the {date} status events must sit in a chunk this step compresses"
    );
    for row in chunks {
        let chunk: String = row.try_get("", "chunk").expect("chunk name column");
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT compress_chunk($1::regclass, if_not_compressed => true)",
            [chunk.clone().into()],
        ))
        .await
        .unwrap_or_else(|e| panic!("compress_chunk({chunk}) failed: {e}"));
    }
}

/// The fixture really is compressed and the pool's cap really is in force: an unguarded bulk write
/// over the same rows must be refused. Without this probe a passing test could be passing on
/// uncompressed rows, which is how this family stayed invisible to the suite.
///
/// The statement is a no-op assignment and it fails, so it leaves no state behind either way.
async fn assert_cap_bites(capped: &DatabaseConnection, table: &str, filter: &str) {
    let sql = format!("UPDATE {table} SET site_id = site_id WHERE {filter}");
    let outcome = capped
        .execute(Statement::from_string(
            DatabaseBackend::Postgres,
            sql.clone(),
        ))
        .await;
    assert!(
        outcome.is_err(),
        "an unguarded `{sql}` must exceed the {CAP}-tuple decompression cap for the rest of this \
         test to mean anything"
    );
}

/// Every tracked job with its status, for assertion messages.
async fn jobs_summary(db: &DatabaseConnection) -> String {
    let rows = db
        .query_all(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT trigger_type, status, retry_count, COALESCE(error_message, '') AS error \
             FROM reprocessing_jobs ORDER BY created_at"
                .to_string(),
        ))
        .await
        .expect("job listing failed");
    rows.iter()
        .map(|r| {
            let trigger: String = r.try_get("", "trigger_type").unwrap_or_default();
            let status: String = r.try_get("", "status").unwrap_or_default();
            let retries: i32 = r.try_get("", "retry_count").unwrap_or_default();
            let error: String = r.try_get("", "error").unwrap_or_default();
            format!("{trigger}={status} (retries {retries}) {error}")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Wait for every tracked job to reach a terminal state, so a background reprocess cannot lift the
/// decompression cap on a fixture between the moment it is compressed and the operation under test.
async fn drain_jobs(db: &DatabaseConnection, max_secs: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(max_secs);
    loop {
        let active = e2e::count(
            db,
            "SELECT count(*) FROM reprocessing_jobs \
             WHERE status IN ('pending', 'queued', 'running', 'retrying')",
        )
        .await;
        if active == 0 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "tracked jobs have not settled after {max_secs}s: {}",
            jobs_summary(db).await
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Poll the newest job of `trigger_type` until it is terminal, returning its status and a summary.
///
/// A failing job returns to `queued` with an exponential retry delay rather than to `failed`, so a
/// timeout reports the last observed state instead of waiting out the whole retry budget.
async fn await_job(db: &DatabaseConnection, trigger_type: &str, max_secs: u64) -> (String, String) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(max_secs);
    loop {
        let row = db
            .query_one(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT status FROM reprocessing_jobs WHERE trigger_type = $1 \
                 ORDER BY created_at DESC LIMIT 1",
                [trigger_type.into()],
            ))
            .await
            .expect("job lookup failed");
        let status: String = row
            .map(|r| r.try_get("", "status").unwrap_or_default())
            .unwrap_or_else(|| "missing".to_string());
        let expired = std::time::Instant::now() >= deadline;
        if status == "completed" || status == "failed" || expired {
            return (status, jobs_summary(db).await);
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

/// Ingest `times` on `stream`, values counting up from `base`.
async fn ingest(app: &Router, jwt: &str, stream: &str, times: &[DateTime<Utc>], base: f64) {
    let readings: Vec<Value> = times
        .iter()
        .enumerate()
        .map(|(i, t)| json!({ "time": t.to_rfc3339(), "raw_value": base + i as f64 }))
        .collect();
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/ingest",
        &json!({ "stream_id": stream, "readings": readings }),
        jwt,
    )
    .await;
    assert_eq!(status, 200, "ingest onto stream {stream}: {body}");
    assert_eq!(
        body["inserted"].as_u64(),
        Some(times.len() as u64),
        "every ingested reading lands: {body}"
    );
}

/// Batch-insert `rows` at one slot, returning the raw response for the caller to assert on.
async fn batch(
    app: &Router,
    jwt: &str,
    site: &str,
    parameter: &str,
    rows: &[(DateTime<Utc>, f64)],
    conflict: &str,
) -> (u16, String) {
    let readings: Vec<Value> = rows
        .iter()
        .map(|(t, v)| {
            json!({
                "site_id": site,
                "parameter_id": parameter,
                "time": t.to_rfc3339(),
                "raw_value": v,
            })
        })
        .collect();
    crate::common::post_json_with_token(
        app,
        "/api/readings/batch",
        &json!({ "readings": readings, "conflict": conflict }),
        jwt,
    )
    .await
}

/// Add a parameter to the catalog and assign it to `site`, returning both ids.
async fn add_slot(app: &Router, jwt: &str, site: &str, code: &str) -> (String, String) {
    let parameter = e2e::create_parameter(app, jwt, code, code, "uM").await;
    let site_parameter = e2e::assign_site_parameter_minimal(app, jwt, site, &parameter).await;
    (parameter, site_parameter)
}

fn readings_for(stream: &str) -> String {
    format!("stream_id = '{stream}'")
}

// unpairing a stream must clear its readings' attribution even when those readings sit in
// compressed chunks, the way its mirror (pairing) already does, and must never leave the stream
// unpaired while its readings keep a site.
#[tokio::test]
#[serial]
async fn unpairing_a_stream_clears_readings_in_compressed_chunks() {
    if !kc::require_keycloak_or_skip("unpairing_a_stream_clears_readings_in_compressed_chunks")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (app, capped) = capped_app().await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_sensor_flow_track(&app, &admin).await;
    let (_manager, river) = operator_jwts(&db, &track.project_id).await;
    let stream = track.stream_ids[0].clone();
    let site_parameter = track.site_parameter_ids[0].clone();

    // Pairing is the mirror of the operation under test and does lift the cap. Proving it succeeds
    // on the same shape of data is what makes the unpair result below a statement about unpair.
    // Both fixtures sit inside Track B's deployment window, so window reprocessing keeps them
    // attributed and the only thing that can un-attribute them is the unpair. An unpaired stream
    // belongs to no project, so only an unrestricted identity can write to it.
    ingest(
        &app,
        &admin,
        &stream,
        &minutes("2025-07-07", COMPRESSED_ROWS),
        10.0,
    )
    .await;
    compress_day(&db, "2025-07-07").await;
    assert_cap_bites(&capped, "readings", &readings_for(&stream)).await;

    let (status, paired) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{stream}/pair"),
        &json!({ "site_parameter_id": site_parameter }),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "pairing lifts the decompression cap for its backfill: {paired}"
    );
    assert_eq!(
        paired["backfilled"].as_u64(),
        Some(COMPRESSED_ROWS as u64),
        "the backfill attributes every compressed row: {paired}"
    );
    drain_jobs(&db, 60).await;

    // A second compressed chunk, written while the stream is paired so its rows land attributed.
    ingest(
        &app,
        &river,
        &stream,
        &minutes("2025-08-04", COMPRESSED_ROWS),
        20.0,
    )
    .await;
    compress_day(&db, "2025-08-04").await;
    assert_cap_bites(
        &capped,
        "readings",
        &format!(
            "{} AND time >= '{}'",
            readings_for(&stream),
            day("2025-08-04").to_rfc3339()
        ),
    )
    .await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream}/unpair"),
        &json!({}),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "unpair must survive compressed history the way pair does: {body}"
    );

    let unpaired: Value = serde_json::from_str(&body).expect("unpair returns JSON");
    assert!(
        unpaired["cleared"].as_u64().unwrap_or(0) >= 1,
        "unpair reports the rows it cleared, and that count is what gates its aggregate \
         refresh: {unpaired}"
    );
    assert!(
        unpaired["stream"]["site_parameter_id"].is_null(),
        "the stream ends unpaired: {unpaired}"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT count(*) FROM readings WHERE stream_id = '{stream}' AND site_id IS NOT NULL"
            )
        )
        .await,
        0,
        "no reading may keep its site once the stream that fed it is unpaired: the pairing state \
         and the readings' attribution have to move together"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT count(*) FROM readings WHERE stream_id = '{stream}'")
        )
        .await,
        2 * COMPRESSED_ROWS as i64,
        "unpairing hides readings from the rollups, it does not delete them"
    );
    assert!(
        e2e::count(
            &db,
            "SELECT count(*) FROM reprocessing_jobs WHERE trigger_type = 'refresh_aggregates_full'"
        )
        .await
            >= 1,
        "clearing attribution must enqueue the refresh that drops those rows from the rollups: {}",
        jobs_summary(&db).await
    );
}

// deleting a calibration or a deployment clears the readings FK with a bulk UPDATE that
// carries neither a time restriction nor the decompression cap lift, so both deletes must still
// succeed over compressed history.
#[tokio::test]
#[serial]
async fn deleting_a_calibration_or_deployment_rewrites_compressed_readings() {
    if !kc::require_keycloak_or_skip("deleting_a_calibration_or_deployment_rewrites_compressed")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (app, capped) = capped_app().await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_sensor_flow_track(&app, &admin).await;
    let (manager, river) = operator_jwts(&db, &track.project_id).await;
    let stream = track.stream_ids[0].clone();
    let sensor = track
        .sensor_id
        .clone()
        .expect("Track B provisions a sensor");
    let deployment = track
        .deployment_id
        .clone()
        .expect("Track B opens a deployment at the site");

    let (status, paired) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{stream}/pair"),
        &json!({ "site_parameter_id": track.site_parameter_ids[0] }),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "pair the stream before any data arrives: {paired}"
    );

    let (status, calibration) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sensor_calibrations",
        &json!({
            "sensor_id": sensor,
            "slope": 2.0,
            "intercept": 5.0,
            "valid_from": "2025-01-01T00:00:00Z",
        }),
        &manager,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "enter a curve covering the history ({status}): {calibration}"
    );
    let calibration = e2e::id_of(&calibration);
    drain_jobs(&db, 60).await;

    // Curve deletion: readings dated inside the curve's window, then compressed.
    ingest(
        &app,
        &river,
        &stream,
        &minutes("2025-05-05", COMPRESSED_ROWS),
        30.0,
    )
    .await;
    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT count(*) FROM readings WHERE calibration_id = '{calibration}'")
        )
        .await,
        COMPRESSED_ROWS as i64,
        "the ingested readings carry the curve, so deleting it has rows to clear"
    );
    compress_day(&db, "2025-05-05").await;
    assert_cap_bites(&capped, "readings", &readings_for(&stream)).await;

    let (status, body) = crate::common::delete_with_token(
        &app,
        &format!("/api/sensor_calibrations/{calibration}"),
        &manager,
    )
    .await;
    assert_eq!(
        status, 204,
        "a curve must be deletable when its readings are compressed: {body}"
    );
    let (status, gone) = crate::common::get_with_token(
        &app,
        &format!("/api/sensor_calibrations/{calibration}"),
        &manager,
    )
    .await;
    assert_eq!(status, 404, "the deleted curve is gone: {gone}");
    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT count(*) FROM readings WHERE calibration_id = '{calibration}'")
        )
        .await,
        0,
        "no reading may still point at a deleted curve"
    );
    drain_jobs(&db, 60).await;

    // Deployment deletion: a second fixture, written and compressed after the delete above, so it
    // meets the operation compressed rather than as a by-product of the previous statement.
    ingest(
        &app,
        &river,
        &stream,
        &minutes("2025-07-07", COMPRESSED_ROWS),
        40.0,
    )
    .await;
    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT count(*) FROM readings WHERE deployment_id = '{deployment}'")
        )
        .await,
        COMPRESSED_ROWS as i64,
        "the second fixture falls inside the deployment window, so the delete has rows to clear"
    );
    compress_day(&db, "2025-07-07").await;
    assert_cap_bites(
        &capped,
        "readings",
        &format!(
            "{} AND time >= '{}'",
            readings_for(&stream),
            day("2025-07-07").to_rfc3339()
        ),
    )
    .await;

    let (status, body) = crate::common::delete_with_token(
        &app,
        &format!("/api/sensor_deployments/{deployment}"),
        &manager,
    )
    .await;
    assert_eq!(
        status, 204,
        "a deployment must be deletable when its readings are compressed, as \
         POST /actions/rollback_deployment already is: {body}"
    );
    let (status, gone) = crate::common::get_with_token(
        &app,
        &format!("/api/sensor_deployments/{deployment}"),
        &manager,
    )
    .await;
    assert_eq!(status, 404, "the deleted deployment is gone: {gone}");
    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT count(*) FROM readings WHERE deployment_id = '{deployment}'")
        )
        .await,
        0,
        "no reading may still point at a deleted deployment"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT count(*) FROM readings WHERE stream_id = '{stream}'")
        )
        .await,
        2 * COMPRESSED_ROWS as i64,
        "clearing the two foreign keys touches no reading's existence"
    );
}

// flag and unflag write in 500-key chunks with no transaction and refresh the aggregates
// only after the loop, so a partial write must not be possible and the refresh must run.
#[tokio::test]
#[serial]
async fn flagging_readings_is_all_or_nothing_and_refreshes_the_rollups() {
    if !kc::require_keycloak_or_skip("flagging_readings_is_all_or_nothing").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (app, capped) = capped_app().await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_csv_track(&app, &admin).await;
    let (_manager, river) = operator_jwts(&db, &track.project_id).await;
    let site = track.site_id.clone();
    let keyed = track.parameter_id("TrkCsvDepth").to_string();
    let ranged = track.parameter_id("TrkCsvTurb").to_string();

    // A slot nobody flags, so an indiscriminate UPDATE fails this test instead of passing it.
    let (untouched, _) = add_slot(&app, &admin, &site, "TrkFlagControl").await;
    let control_rows: Vec<(DateTime<Utc>, f64)> = minutes("2025-09-01", 10)
        .into_iter()
        .map(|t| (t, 1.0))
        .collect();
    let (status, body) = batch(&app, &river, &site, &untouched, &control_rows, "skip").await;
    assert_eq!(status, 200, "control slot ingested: {body}");

    // Keyed arm: 500 keys in an uncompressed chunk followed by 200 in a compressed one, so the
    // first 500-key statement can commit before the second one meets the cap.
    let cheap = minutes("2025-09-01", 500);
    let compressed = minutes("2025-03-03", 200);
    let keyed_rows: Vec<(DateTime<Utc>, f64)> = cheap
        .iter()
        .chain(compressed.iter())
        .map(|t| (*t, 7.0))
        .collect();
    let (status, body) = batch(&app, &river, &site, &keyed, &keyed_rows, "skip").await;
    assert_eq!(status, 200, "keyed-arm readings ingested: {body}");
    drain_jobs(&db, 60).await;
    compress_day(&db, "2025-03-03").await;
    assert_cap_bites(
        &capped,
        "readings",
        &format!(
            "parameter_id = '{keyed}' AND time < '{}'",
            day("2025-04-01").to_rfc3339()
        ),
    )
    .await;

    let keys: Vec<Value> = cheap
        .iter()
        .chain(compressed.iter())
        .map(|t| json!({ "site_id": site, "parameter_id": keyed, "time": t.to_rfc3339() }))
        .collect();
    let (status, body) = crate::common::patch_json_with_token(
        &app,
        "/api/readings/flag",
        &json!({ "readings": keys, "reason": "sensor fouling" }),
        &river,
    )
    .await;
    assert_eq!(
        status, 200,
        "a flag set spanning more than one 500-key chunk must apply whole: {body}"
    );
    let flagged: Value = serde_json::from_str(&body).expect("flag returns JSON");
    assert_eq!(
        flagged["updated"].as_u64(),
        Some(700),
        "every key in the request is flagged: {flagged}"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT count(*) FROM readings WHERE parameter_id = '{keyed}' AND is_flagged = TRUE"
            )
        )
        .await,
        700,
        "a chunked write either flags every key or none: a partial set is the state no reader can \
         interpret"
    );

    // Range arm: one unpruned statement over a compressed window, plus the rollup it must repair.
    let range_times = quarter_minutes("2025-04-07", 200);
    let range_rows: Vec<(DateTime<Utc>, f64)> = range_times.iter().map(|t| (*t, 10.0)).collect();
    let (status, body) = batch(&app, &river, &site, &ranged, &range_rows, "skip").await;
    assert_eq!(status, 200, "range-arm readings ingested: {body}");
    drain_jobs(&db, 60).await;
    compress_day(&db, "2025-04-07").await;
    assert_cap_bites(&capped, "readings", &format!("parameter_id = '{ranged}'")).await;

    e2e::refresh_hourly(&db, day("2025-01-01")).await;
    assert_eq!(
        e2e::hourly_bucket(&db, &site, &ranged, day("2025-04-07")).await,
        Some((10.0, 200)),
        "the bucket is materialised before flagging, so its disappearance afterwards is evidence"
    );

    let (status, body) = crate::common::patch_json_with_token(
        &app,
        "/api/readings/flag_range",
        &json!({
            "site_id": site,
            "parameter_id": ranged,
            "start_time": day("2025-04-07").to_rfc3339(),
            "end_time": (day("2025-04-07") + Duration::hours(1)).to_rfc3339(),
            "reason": "calibration drift",
        }),
        &river,
    )
    .await;
    assert_eq!(
        status, 200,
        "flagging a historical window must not fail because the window is compressed: {body}"
    );
    let flagged: Value = serde_json::from_str(&body).expect("flag_range returns JSON");
    assert_eq!(
        flagged["updated"].as_u64(),
        Some(200),
        "every reading in the window is flagged: {flagged}"
    );
    assert_eq!(
        e2e::hourly_bucket(&db, &site, &ranged, day("2025-04-07")).await,
        None,
        "the rollup is refreshed after flagging, so the flagged hour stops being served"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT count(*) FROM readings WHERE parameter_id = '{untouched}' \
                 AND is_flagged = TRUE"
            )
        )
        .await,
        0,
        "no reading outside the two requests is flagged"
    );
}

// merge_site_parameters runs five mutations with no transaction, so a failure part-way
// must not leave readings relabelled while the streams and the source slot still exist.
#[tokio::test]
#[serial]
async fn merging_site_parameters_applies_every_step_or_none() {
    if !kc::require_keycloak_or_skip("merging_site_parameters_applies_every_step_or_none").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (app, capped) = capped_app().await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_sensor_flow_track(&app, &admin).await;
    let (manager, river) = operator_jwts(&db, &track.project_id).await;
    let site = track.site_id.clone();
    let stream = track.stream_ids[0].clone();
    let source_slot = track.site_parameter_ids[0].clone();
    let source_parameter = track.parameter_id("TrkFlowDO").to_string();
    let (target_parameter, target_slot) = add_slot(&app, &admin, &site, "TrkMergeTarget").await;
    let (bystander_parameter, _) = add_slot(&app, &admin, &site, "TrkMergeBystander").await;

    let (status, paired) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{stream}/pair"),
        &json!({ "site_parameter_id": source_slot }),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "the source slot is fed by a paired stream: {paired}"
    );

    // Readings stay in an uncompressed chunk so the merge's first statement commits; the status
    // events are compressed so its second statement is the one that meets the cap.
    ingest(&app, &river, &stream, &minutes("2025-09-01", 5), 50.0).await;
    let bystander_rows: Vec<(DateTime<Utc>, f64)> = minutes("2025-09-01", 3)
        .into_iter()
        .map(|t| (t, 1.0))
        .collect();
    let (status, body) = batch(
        &app,
        &river,
        &site,
        &bystander_parameter,
        &bystander_rows,
        "skip",
    )
    .await;
    assert_eq!(status, 200, "bystander slot ingested: {body}");

    let events: Vec<Value> = minutes("2025-03-03", 60)
        .iter()
        .map(|t| {
            json!({
                "site_id": site,
                "parameter_id": source_parameter,
                "time": t.to_rfc3339(),
                "value": "offline",
            })
        })
        .collect();
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/status_events/batch",
        &json!({ "events": events }),
        &river,
    )
    .await;
    assert_eq!(
        status, 200,
        "status events ingested on the source slot: {body}"
    );
    drain_jobs(&db, 60).await;
    compress_status_events_day(&db, "2025-03-03").await;
    assert_cap_bites(
        &capped,
        "status_events",
        &format!("parameter_id = '{source_parameter}'"),
    )
    .await;

    let (status, queued) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/merge_site_parameters",
        &json!({
            "source_site_parameter_id": source_slot,
            "target_site_parameter_id": target_slot,
        }),
        &manager,
    )
    .await;
    assert_eq!(status, 200, "the merge is accepted and queued: {queued}");

    let (job_status, summary) = await_job(&db, "merge_site_parameters", 30).await;
    assert_eq!(
        job_status, "completed",
        "the merge must run to completion rather than stop half-applied: {summary}"
    );

    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT count(*) FROM readings WHERE parameter_id = '{target_parameter}'")
        )
        .await,
        5,
        "the source's readings end up on the target parameter"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT count(*) FROM status_events WHERE parameter_id = '{target_parameter}'"
            )
        )
        .await,
        60,
        "the source's status events move with its readings, or the two disagree about which \
         parameter the slot's history belongs to"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT count(*) FROM status_events WHERE parameter_id = '{source_parameter}'"
            )
        )
        .await,
        0,
        "nothing is left behind on the absorbed parameter"
    );

    let (status, updated) =
        crate::common::get_json_with_token(&app, &format!("/api/data_streams/{stream}"), &admin)
            .await;
    assert_eq!(status, 200, "read the merged stream back: {updated}");
    assert_eq!(
        updated["site_parameter_id"].as_str(),
        Some(target_slot.as_str()),
        "the stream feeding the source slot now feeds the target: {updated}"
    );

    let (status, gone) =
        crate::common::get_with_token(&app, &format!("/api/site_parameters/{source_slot}"), &admin)
            .await;
    assert_eq!(status, 404, "the absorbed slot is deleted: {gone}");

    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT count(*) FROM readings WHERE parameter_id = '{bystander_parameter}'")
        )
        .await,
        3,
        "a merge relabels only the two slots it names"
    );
}

// POST /readings/batch in overwrite mode upserts in 1000-row chunks with neither a
// transaction nor the cap lift /ingest applies, so a re-import spanning compressed history must
// replace every row rather than the first chunk only.
#[tokio::test]
#[serial]
async fn batch_overwrite_replaces_readings_in_compressed_chunks() {
    if !kc::require_keycloak_or_skip("batch_overwrite_replaces_readings_in_compressed_chunks").await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (app, capped) = capped_app().await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_csv_track(&app, &admin).await;
    let (_manager, river) = operator_jwts(&db, &track.project_id).await;
    let site = track.site_id.clone();
    let parameter = track.parameter_id("TrkCsvDepth").to_string();
    let bystander = track.parameter_id("TrkCsvTurb").to_string();

    // 1000 rows in an uncompressed chunk, then 200 in a chunk that will be compressed: the first
    // 1000-row upsert can commit before the second one meets the cap.
    let cheap = minutes("2025-09-01", 1000);
    let compressed = minutes("2025-03-03", 200);
    let original: Vec<(DateTime<Utc>, f64)> = cheap
        .iter()
        .chain(compressed.iter())
        .enumerate()
        .map(|(i, t)| (*t, 100.0 + i as f64))
        .collect();
    let (status, body) = batch(&app, &river, &site, &parameter, &original, "skip").await;
    assert_eq!(status, 200, "the original import lands: {body}");
    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT count(*) FROM readings WHERE parameter_id = '{parameter}'")
        )
        .await,
        1200,
        "the fixture spans both chunks, so the correction below has to cross the compressed one"
    );

    let bystander_rows: Vec<(DateTime<Utc>, f64)> = minutes("2025-09-01", 5)
        .into_iter()
        .map(|t| (t, 1.0))
        .collect();
    let (status, body) = batch(&app, &river, &site, &bystander, &bystander_rows, "skip").await;
    assert_eq!(status, 200, "bystander slot ingested: {body}");

    drain_jobs(&db, 60).await;
    compress_day(&db, "2025-03-03").await;
    assert_cap_bites(
        &capped,
        "readings",
        &format!(
            "parameter_id = '{parameter}' AND time < '{}'",
            day("2025-04-01").to_rfc3339()
        ),
    )
    .await;

    let corrected: Vec<(DateTime<Utc>, f64)> =
        original.iter().map(|(t, v)| (*t, v + 5000.0)).collect();
    let (status, body) = batch(&app, &river, &site, &parameter, &corrected, "overwrite").await;
    assert_eq!(
        status, 200,
        "a corrected re-import must replace history that has since been compressed: {body}"
    );
    let replaced: Value = serde_json::from_str(&body).expect("batch returns JSON");
    assert_eq!(
        replaced["overwritten"].as_u64(),
        Some(1200),
        "every submitted row replaces its stored twin: {replaced}"
    );
    assert_eq!(
        replaced["inserted"].as_u64(),
        Some(0),
        "a pure correction inserts nothing new: {replaced}"
    );

    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT count(*) FROM readings WHERE parameter_id = '{parameter}' \
                 AND raw_value < 5000"
            )
        )
        .await,
        0,
        "no row keeps its pre-correction value: a chunked upsert that stops part-way leaves a file \
         half-applied with nothing recording the boundary"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT count(*) FROM readings WHERE parameter_id = '{parameter}' \
                 AND time < '{}' AND raw_value >= 5000",
                day("2025-04-01").to_rfc3339()
            )
        )
        .await,
        200,
        "the compressed chunk's rows are the ones that must carry the correction"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT count(*) FROM readings WHERE parameter_id = '{bystander}' \
                 AND raw_value = 1"
            )
        )
        .await,
        5,
        "an overwrite touches only the keys it names"
    );
}

// pair_stream reads the stream without a claim and commits the pairing before its backfill,
// so two pairings racing for one stream must not both win.
#[tokio::test]
#[serial]
async fn concurrent_pairings_of_one_stream_leave_a_single_winner() {
    if !kc::require_keycloak_or_skip("concurrent_pairings_of_one_stream_leave_a_single_winner")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_sensor_flow_track(&app, &admin).await;
    let site = track.site_id.clone();
    let stream = track.stream_ids[0].clone();
    let (_second_parameter, second_slot) = add_slot(&app, &admin, &site, "TrkRaceSecond").await;
    let (_third_parameter, third_slot) = add_slot(&app, &admin, &site, "TrkRaceThird").await;

    // An unpaired stream belongs to no project, so only an unrestricted identity can write to it.
    let times = minutes("2025-06-02", 6);
    ingest(&app, &admin, &stream, &times, 60.0).await;

    let path = format!("/api/streams/{stream}/pair");
    let first = json!({ "site_parameter_id": track.site_parameter_ids[0] });
    let second = json!({ "site_parameter_id": second_slot });
    let third = json!({ "site_parameter_id": third_slot });
    let (a, b, c) = tokio::join!(
        crate::common::post_json_with_token(&app, &path, &first, &admin),
        crate::common::post_json_with_token(&app, &path, &second, &admin),
        crate::common::post_json_with_token(&app, &path, &third, &admin),
    );

    let outcomes = [a, b, c];
    let winners = outcomes
        .iter()
        .filter(|(status, _)| (200..300).contains(status))
        .count();
    assert_eq!(
        winners, 1,
        "one stream can only be paired once: pairing has to claim the stream before it works, \
         not read it and hope. Outcomes: {outcomes:?}"
    );
    for (status, body) in &outcomes {
        assert!(
            (200..300).contains(status) || (400..500).contains(status),
            "a losing pair request is refused, not a server error ({status}): {body}"
        );
    }

    drain_jobs(&db, 60).await;

    let (status, stream_row) =
        crate::common::get_json_with_token(&app, &format!("/api/data_streams/{stream}"), &admin)
            .await;
    assert_eq!(status, 200, "read the stream back: {stream_row}");
    let winning_slot = stream_row["site_parameter_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the stream ends paired: {stream_row}"))
        .to_string();

    let (status, slot) = crate::common::get_json_with_token(
        &app,
        &format!("/api/site_parameters/{winning_slot}"),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "read the winning slot back: {slot}");
    let winning_parameter = slot["parameter_id"]
        .as_str()
        .unwrap_or_else(|| panic!("a site_parameter carries a parameter: {slot}"))
        .to_string();

    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT count(*) FROM readings WHERE stream_id = '{stream}' \
                 AND parameter_id = '{winning_parameter}'"
            )
        )
        .await,
        times.len() as i64,
        "the readings carry the parameter the stream says it is paired to; a backfill that ran for \
         a pairing another request has already overwritten attributes them to nobody"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT count(DISTINCT parameter_id) FROM readings WHERE stream_id = '{stream}'"
            )
        )
        .await,
        1,
        "one stream's history belongs to exactly one parameter"
    );
}
