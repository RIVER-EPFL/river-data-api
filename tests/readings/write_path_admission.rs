//! The admission rules the reading write paths share, at their boundaries.
//!
//! `/ingest`, `/readings/batch`, `/grab_samples` and the CSV importer all pass their rows through
//! one admission check, and all upsert through one conflict clause. These cover the edges that
//! check has to get right: the exact window edge, the value next to the missing-value sentinel, a
//! file where every cell is missing, a correction whose sample holds a single replicate, and an
//! import that overlaps a slot another stream already feeds.
//!
//! Run: cargo test --test readings -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::e2e;
use crate::common::{GLOBAL_PARAM_DO_ID, SITE1_ID};

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

async fn register_stream(app: &axum::Router, token: &str, key: &str) -> String {
    let (status, stream) = crate::common::post_json_parse_with_token(
        app,
        "/api/streams/register",
        &json!({ "source_system": "admission", "source_key": key }),
        token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "register ({status}): {stream}"
    );
    e2e::id_of(&stream)
}

async fn scalar_f64(db: &DatabaseConnection, sql: &str) -> Option<f64> {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .expect("query")
    .and_then(|row| row.try_get::<f64>("", "v").ok())
}

fn seconds_rfc3339(at: chrono::DateTime<chrono::Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

async fn import(app: &axum::Router, token: &str, body: &serde_json::Value) -> serde_json::Value {
    let (status, resp) =
        crate::common::post_json_parse_with_token(app, "/api/readings/import_csv", body, token)
            .await;
    assert_eq!(status, 200, "csv import ({status}): {resp}");
    resp
}

/// Scenario: the same reading is offered to every JSON write path.
///
/// Expected behaviour: they agree on which timestamps are admissible, and disagree on what to do
/// about an inadmissible one. `/readings/batch` and `/grab_samples` refuse the request, because
/// their caller can correct it and resubmit. `/ingest` skips the reading and reports it, because
/// its caller replays from a cursor that only advances on success: refusing would block the head
/// of that queue forever.
///
/// The past bound is an absolute instant, not `now - N years`. The floor itself is absolute, and a
/// relative literal here would re-encode the moving bound the floor exists to avoid.
#[tokio::test]
#[serial]
async fn ingest_skips_what_batch_and_grab_samples_refuse() {
    let (db, app, token) = setup().await;
    let stream = register_stream(&app, &token, "window").await;

    let now = chrono::Utc::now();
    let outside = [
        ("future", seconds_rfc3339(now + chrono::Duration::days(30))),
        ("past", "1999-12-31T23:59:59Z".to_string()),
    ];

    for (label, at) in &outside {
        let (status, body) = crate::common::post_json_parse_with_token(
            &app,
            "/api/ingest",
            &json!({ "stream_id": stream, "readings": [{ "time": at, "raw_value": 42.0 }] }),
            &token,
        )
        .await;
        assert_eq!(
            status, 200,
            "/ingest accepts the request and skips a {label} timestamp ({status}): {body}"
        );
        assert_eq!(body["inserted"], 0, "nothing lands for {label}: {body}");
        assert_eq!(body["skipped"], 1, "the {label} reading is counted: {body}");

        let (status, body) = crate::common::post_json_with_token(
            &app,
            "/api/readings/batch",
            &json!({
                "readings": [{
                    "site_id": SITE1_ID,
                    "parameter_id": GLOBAL_PARAM_DO_ID,
                    "time": at,
                    "raw_value": 42.0,
                }]
            }),
            &token,
        )
        .await;
        assert_eq!(
            status, 400,
            "/readings/batch refuses a {label} timestamp ({status}): {body}"
        );

        let (status, body) = crate::common::post_json_with_token(
            &app,
            "/api/grab_samples",
            &json!({
                "site_id": SITE1_ID,
                "readings": [{
                    "parameter_id": GLOBAL_PARAM_DO_ID,
                    "time": at,
                    "value": 42.0,
                }]
            }),
            &token,
        )
        .await;
        assert_eq!(
            status, 400,
            "/grab_samples refuses a {label} timestamp ({status}): {body}"
        );

        let stored = e2e::count(
            &db,
            &format!("SELECT count(*) FROM readings WHERE time = '{at}'"),
        )
        .await;
        assert_eq!(
            stored, 0,
            "an inadmissible {label} timestamp stores nothing"
        );
    }

    let (status, stream_row) =
        crate::common::get_json_with_token(&app, &format!("/api/data_streams/{stream}"), &token)
            .await;
    assert_eq!(status, 200, "read the stream back ({status}): {stream_row}");
    assert!(
        stream_row["last_data_time"].is_null(),
        "the cursor is computed from surviving readings, so an all-skipped ingest does not move \
         it, and a future timestamp in particular cannot latch it: {stream_row}"
    );

    // The point of skipping rather than refusing: one bad reading must not cost the good ones it
    // shared a batch with, and the cursor must land on the newest survivor.
    let good_early = seconds_rfc3339(now - chrono::Duration::hours(2));
    let good_late = seconds_rfc3339(now - chrono::Duration::hours(1));
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &json!({
            "stream_id": stream,
            "readings": [
                { "time": "1999-01-01T00:00:00Z", "raw_value": 1.0 },
                { "time": good_early, "raw_value": 2.0 },
                { "time": good_late, "raw_value": 3.0 },
            ],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "a mixed batch is accepted ({status}): {body}");
    assert_eq!(body["inserted"], 2, "both admissible readings land: {body}");
    assert_eq!(body["skipped"], 1, "only the bad one is dropped: {body}");

    let (_, stream_row) =
        crate::common::get_json_with_token(&app, &format!("/api/data_streams/{stream}"), &token)
            .await;
    assert_eq!(
        stream_row["last_data_time"]
            .as_str()
            .map(|t| t.parse::<chrono::DateTime<chrono::Utc>>().unwrap()),
        Some(good_late.parse::<chrono::DateTime<chrono::Utc>>().unwrap()),
        "the cursor advances to the newest surviving reading: {stream_row}"
    );

    // One second inside the window is inside it: the bound refuses only what falls outside.
    let inside = seconds_rfc3339(now + chrono::Duration::hours(23));
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &json!({ "stream_id": stream, "readings": [{ "time": inside, "raw_value": 42.0 }] }),
        &token,
    )
    .await;
    assert_eq!(
        status, 200,
        "an in-window ingest is accepted ({status}): {body}"
    );
    assert_eq!(body["inserted"], 1, "the in-window reading lands: {body}");
}

/// A CSV row outside the window is reported against its line, and the rest of the file imports.
#[tokio::test]
#[serial]
async fn a_csv_row_outside_the_window_is_a_row_error_and_the_file_still_imports() {
    let (db, app, token) = setup().await;

    let csv = "DateTime,Dissolved_O2\n\
               2025-07-01 00:00:00,250\n\
               1995-07-01 00:00:00,260\n\
               2025-07-01 00:10:00,270\n";

    let resp = import(
        &app,
        &token,
        &json!({ "site": SITE1_ID, "csv": csv, "dry_run": false }),
    )
    .await;
    assert_eq!(
        resp["error_count"], 1,
        "one row is outside the window: {resp}"
    );
    assert_eq!(
        resp["row_count"], 2,
        "the two admissible rows are the ones counted: {resp}"
    );

    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "csv_import", 30).await,
        "the csv_import job runs and succeeds"
    );

    let imported = e2e::count(
        &db,
        &format!(
            "SELECT count(*) FROM readings WHERE site_id = '{SITE1_ID}' \
             AND parameter_id = '{GLOBAL_PARAM_DO_ID}' AND time >= '2025-07-01T00:00:00Z'"
        ),
    )
    .await;
    assert_eq!(imported, 2, "the admissible rows import");

    let out_of_window = e2e::count(
        &db,
        "SELECT count(*) FROM readings WHERE time < '2015-01-01T00:00:00Z'",
    )
    .await;
    assert_eq!(out_of_window, 0, "the out-of-window row is not stored");
}

/// The sentinel is a value, so the values either side of it are measurements.
#[tokio::test]
#[serial]
async fn the_sentinel_is_recognised_by_value_and_its_neighbours_are_measurements() {
    let (db, app, token) = setup().await;

    let csv = "DateTime,Dissolved_O2\n\
               2025-07-02 00:00:00,-9999.00\n\
               2025-07-02 00:10:00,-9999.5\n\
               2025-07-02 00:20:00,-9998\n\
               2025-07-02 00:30:00,250\n";

    let resp = import(
        &app,
        &token,
        &json!({ "site": SITE1_ID, "csv": csv, "dry_run": false }),
    )
    .await;
    assert_eq!(
        resp["error_count"], 0,
        "a sentinel is a declared missing value: {resp}"
    );

    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "csv_import", 30).await,
        "the csv_import job runs and succeeds"
    );

    let window = format!(
        "site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}' \
         AND time >= '2025-07-02T00:00:00Z' AND time < '2025-07-03T00:00:00Z'"
    );
    let stored = e2e::count(
        &db,
        &format!("SELECT count(*) FROM readings WHERE {window}"),
    )
    .await;
    assert_eq!(
        stored, 3,
        "the sentinel row is missing, the other three are measurements"
    );

    let sentinel = e2e::count(
        &db,
        &format!("SELECT count(*) FROM readings WHERE {window} AND raw_value = -9999"),
    )
    .await;
    assert_eq!(
        sentinel, 0,
        "no spelling of the sentinel is stored as a measurement"
    );

    let neighbour = scalar_f64(
        &db,
        &format!(
            "SELECT raw_value AS v FROM readings WHERE {window} \
             AND time = '2025-07-02T00:10:00Z'"
        ),
    )
    .await;
    assert_eq!(
        neighbour,
        Some(-9999.5),
        "a value next to the sentinel is a measurement, not the marker"
    );
}

/// A file whose every cell is missing parses, reports its rows, and writes nothing.
#[tokio::test]
#[serial]
async fn a_file_of_declared_missing_cells_imports_nothing_and_raises_nothing() {
    let (db, app, token) = setup().await;

    let csv = "DateTime,Dissolved_O2\n\
               2025-07-03 00:00:00,NaN\n\
               2025-07-03 00:10:00,\n\
               2025-07-03 00:20:00,-9999\n";

    let resp = import(
        &app,
        &token,
        &json!({ "site": SITE1_ID, "csv": csv, "dry_run": false }),
    )
    .await;
    assert_eq!(
        resp["error_count"], 0,
        "missing cells are not row errors: {resp}"
    );
    assert_eq!(resp["row_count"], 3, "the rows parsed: {resp}");
    assert_eq!(resp["inserted_total"], 0, "nothing was ingestible: {resp}");
    assert!(
        resp["derived_job_id"].is_null(),
        "a file with no values queues no work: {resp}"
    );

    let stored = e2e::count(
        &db,
        &format!(
            "SELECT count(*) FROM readings WHERE site_id = '{SITE1_ID}' \
             AND time >= '2025-07-03T00:00:00Z' AND time < '2025-07-04T00:00:00Z'"
        ),
    )
    .await;
    assert_eq!(stored, 0, "no reading is written for a missing cell");
}

/// Correcting the only reading a sample holds keeps the sample: the link is not part of the
/// correction, and a sample nothing references is deleted by the refresh.
#[tokio::test]
#[serial]
async fn correcting_a_lone_replicate_keeps_its_sample() {
    let (_db, app, token) = setup().await;

    let at = "2025-07-04T09:00:00Z";
    let (status, sample) = crate::common::post_json_parse_with_token(
        &app,
        "/api/samples",
        &json!({
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_DO_ID,
            "collected_at": at,
            "label": "lone bottle",
            "notes": "one replicate",
            "created_by": "lab",
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "declare the sample ({status}): {sample}"
    );
    let sample_id = e2e::id_of(&sample);

    let reading = |raw: f64, link: bool| {
        let mut r = json!({
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_DO_ID,
            "time": at,
            "raw_value": raw,
            "measurement_type": "spot",
        });
        if link {
            r["sample_id"] = json!(sample_id);
        }
        r
    };

    let (status, inserted) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/batch",
        &json!({ "readings": [reading(250.0, true)] }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "insert the replicate ({status}): {inserted}");

    let (status, corrected) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/batch",
        &json!({ "conflict": "overwrite", "readings": [reading(260.0, false)] }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "correct the replicate ({status}): {corrected}");
    assert_eq!(
        corrected["overwritten"], 1,
        "the stored row is replaced: {corrected}"
    );

    let (status, sample) =
        crate::common::get_json_with_token(&app, &format!("/api/samples/{sample_id}"), &token)
            .await;
    assert_eq!(
        status, 200,
        "the sample survives a correction that omits the link ({status}): {sample}"
    );
    assert_eq!(sample["n"], 1, "its one replicate still counts: {sample}");
    assert_eq!(
        sample["label"], "lone bottle",
        "the label survives: {sample}"
    );
    assert_eq!(
        sample["mean"].as_f64(),
        Some(260.0),
        "the statistics follow the corrected value: {sample}"
    );
}

/// An import that partly overlaps a slot another stream already feeds writes each row where the
/// slot's data lives: onto the stored row when there is one, onto the importer's own stream when
/// there is not. In `skip` mode the overlap is left alone rather than duplicated beside it.
#[tokio::test]
#[serial]
async fn an_import_overlapping_another_streams_slot_writes_onto_the_stored_row() {
    let (db, app, token) = setup().await;

    let occupied = "2025-01-15T00:00:00Z";
    let empty = "2025-01-15T00:05:00Z";
    let slot = format!("site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}'");
    let seeded = scalar_f64(
        &db,
        &format!("SELECT raw_value AS v FROM readings WHERE {slot} AND time = '{occupied}'"),
    )
    .await
    .expect("the seed writes a Dissolved_O2 reading at the start of its window");

    let csv = "DateTime,Dissolved_O2\n\
               2025-01-15 00:00:00,777\n\
               2025-01-15 00:05:00,888\n";

    let skipped = import(
        &app,
        &token,
        &json!({ "site": SITE1_ID, "csv": csv, "dry_run": false }),
    )
    .await;
    assert_eq!(
        skipped["overlaps_differing"], 1,
        "one row overlaps the seeded slot: {skipped}"
    );
    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "csv_import", 30).await,
        "the csv_import job runs and succeeds"
    );

    let at_occupied = e2e::count(
        &db,
        &format!("SELECT count(*) FROM readings WHERE {slot} AND time = '{occupied}'"),
    )
    .await;
    assert_eq!(
        at_occupied, 1,
        "in skip mode the overlap is dropped, not written a second time on another stream"
    );
    assert_eq!(
        scalar_f64(
            &db,
            &format!("SELECT raw_value AS v FROM readings WHERE {slot} AND time = '{occupied}'")
        )
        .await,
        Some(seeded),
        "the stored value is untouched in skip mode"
    );
    assert_eq!(
        scalar_f64(
            &db,
            &format!("SELECT raw_value AS v FROM readings WHERE {slot} AND time = '{empty}'")
        )
        .await,
        Some(888.0),
        "the row with no stored counterpart is inserted"
    );

    let overwritten = import(
        &app,
        &token,
        &json!({ "site": SITE1_ID, "csv": csv, "dry_run": false, "conflict": "overwrite" }),
    )
    .await;
    assert_eq!(
        overwritten["overwritten"], 1,
        "the operator is told one stored reading was replaced: {overwritten}"
    );
    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "csv_import", 30).await,
        "the second csv_import job runs and succeeds"
    );

    let at_occupied = e2e::count(
        &db,
        &format!("SELECT count(*) FROM readings WHERE {slot} AND time = '{occupied}'"),
    )
    .await;
    assert_eq!(
        at_occupied, 1,
        "an overwrite leaves one reading in the slot"
    );
    assert_eq!(
        scalar_f64(
            &db,
            &format!("SELECT raw_value AS v FROM readings WHERE {slot} AND time = '{occupied}'")
        )
        .await,
        Some(777.0),
        "the correction replaced the value the sync stream had stored"
    );
}
