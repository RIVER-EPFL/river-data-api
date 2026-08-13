//! What the write paths accept that they should refuse, and what they drop that they should keep.
//!
//! Each test asserts the intended behaviour, so a red run is the evidence that the defect is real
//! and a later green run is the evidence that the fix works. Every flow is provisioned from nothing
//! over HTTP as a real Keycloak user, in the order the dashboard drives it.

use axum::Router;
use serde_json::json;
use serial_test::serial;

use crate::common::e2e;
use crate::common::keycloak as kc;
use crate::common::tracks;

const DUPLICATE_TIMESTAMPS: &str = include_str!("../fixtures/viewlinc_duplicate_timestamps.csv");
/// The timestamp the viewLinc fixture already repeats sits on all-NaN rows, so the duplicate that
/// carries values is made by repeating this data line.
const FIXTURE_VALUE_ROW: &str = "2025-04-23 12:00:00";

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

/// One provisioned (site, parameter) slot, the unit every ingest path writes into.
struct Slot {
    site_id: String,
    parameter_id: String,
    site_parameter_id: String,
}

async fn provision_slot(app: &Router, jwt: &str, slug: &str, code: &str) -> Slot {
    let project_id = e2e::create_project(
        app,
        jwt,
        &format!("Ingest {slug}"),
        &format!("ing-{slug}"),
        true,
    )
    .await;
    let site_id = e2e::create_site(
        app,
        jwt,
        &project_id,
        &format!("Ingest site {slug}"),
        &format!("site-ing-{slug}"),
    )
    .await;
    let parameter_id =
        e2e::create_parameter(app, jwt, code, &format!("Ingest {slug}"), "ppb").await;
    let site_parameter_id =
        e2e::assign_site_parameter_minimal(app, jwt, &site_id, &parameter_id).await;
    Slot {
        site_id,
        parameter_id,
        site_parameter_id,
    }
}

/// Register a source-system stream and pair it to the slot, the sync-service provenance.
async fn register_and_pair(
    app: &Router,
    jwt: &str,
    source_system: &str,
    source_key: &str,
    site_parameter_id: &str,
) -> String {
    let (status, stream) = crate::common::post_json_parse_with_token(
        app,
        "/api/streams/register",
        &json!({
            "source_system": source_system,
            "source_key": source_key,
            "source_name": "Ingest validation stream",
        }),
        jwt,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "register stream {source_key} ({status}): {stream}"
    );
    let stream_id = e2e::id_of(&stream);

    let (status, paired) = crate::common::post_json_parse_with_token(
        app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({ "site_parameter_id": site_parameter_id }),
        jwt,
    )
    .await;
    assert_eq!(status, 200, "pair stream {stream_id} ({status}): {paired}");
    stream_id
}

async fn import_csv(app: &Router, jwt: &str, body: &serde_json::Value) -> serde_json::Value {
    let (status, resp) =
        crate::common::post_json_parse_with_token(app, "/api/readings/import_csv", body, jwt).await;
    assert_eq!(status, 200, "csv import ({status}): {resp}");
    resp
}

/// Declare a sample at a slot, the row a lab plate's replicates are then linked to.
async fn declare_sample(
    app: &Router,
    jwt: &str,
    slot: &Slot,
    collected_at: &str,
    label: &str,
) -> String {
    let (status, sample) = crate::common::post_json_parse_with_token(
        app,
        "/api/samples",
        &json!({
            "site_id": slot.site_id,
            "parameter_id": slot.parameter_id,
            "collected_at": collected_at,
            "label": label,
            "notes": "replicate plate",
            "created_by": "lab",
        }),
        jwt,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "declare the sample collected at {collected_at} ({status}): {sample}"
    );
    e2e::id_of(&sample)
}

async fn sample_row(app: &Router, jwt: &str, sample_id: &str, context: &str) -> serde_json::Value {
    let (status, sample) =
        crate::common::get_json_with_token(app, &format!("/api/samples/{sample_id}"), jwt).await;
    assert_eq!(
        status, 200,
        "{context}: the sample must still exist ({status}): {sample}"
    );
    sample
}

fn f64_at(value: &serde_json::Value, key: &str) -> f64 {
    value[key]
        .as_f64()
        .unwrap_or_else(|| panic!("'{key}' is not a number in {value}"))
}

fn instant(rfc3339: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .unwrap_or_else(|e| panic!("time {rfc3339} is not RFC 3339: {e}"))
        .with_timezone(&chrono::Utc)
}

fn seconds_rfc3339(at: chrono::DateTime<chrono::Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The hourly bucket a slot resolves at `at`, asserted present, as `(mean, count)`.
async fn bucket(
    db: &sea_orm::DatabaseConnection,
    site_id: &str,
    parameter_id: &str,
    at: chrono::DateTime<chrono::Utc>,
    context: &str,
) -> (f64, i64) {
    let materialised = e2e::hourly_bucket(db, site_id, parameter_id, at).await;
    assert!(
        materialised.is_some(),
        "{context}: the hour holding the imported rows must materialise a bucket"
    );
    materialised.unwrap()
}

/// a `conflict: "overwrite"` correction must keep the corrected reading's sample link, so
/// the sample survives with every replicate still counted.
#[tokio::test]
#[serial]
async fn batch_overwrite_keeps_the_sample_link_of_the_reading_it_corrects() {
    if !kc::require_keycloak_or_skip("batch_overwrite_keeps_the_sample_link").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let slot = provision_slot(&app, &admin, "ovw", "IngOvwDoc").await;
    let corrected_at = "2025-06-10T09:00:00Z";
    let untouched_at = "2025-06-10T10:00:00Z";

    let corrected_sample = declare_sample(&app, &admin, &slot, corrected_at, "morning plate").await;
    let untouched_sample = declare_sample(&app, &admin, &slot, untouched_at, "midday plate").await;

    let replicate = |at: &str, index: i16, raw: f64, sample_id: Option<&str>| {
        let mut reading = json!({
            "site_id": slot.site_id,
            "parameter_id": slot.parameter_id,
            "time": at,
            "raw_value": raw,
            "replicate_index": index,
            "measurement_type": "spot",
        });
        if let Some(id) = sample_id {
            reading["sample_id"] = json!(id);
        }
        reading
    };

    let (status, inserted) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/batch",
        &json!({
            "readings": [
                replicate(corrected_at, 0, 10.0, Some(corrected_sample.as_str())),
                replicate(corrected_at, 1, 12.0, Some(corrected_sample.as_str())),
                replicate(untouched_at, 0, 10.0, Some(untouched_sample.as_str())),
                replicate(untouched_at, 1, 12.0, Some(untouched_sample.as_str())),
            ]
        }),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "batch insert of both plates ({status}): {inserted}"
    );
    assert_eq!(
        inserted["inserted"], 4,
        "all four replicates land: {inserted}"
    );

    let sample = sample_row(&app, &admin, &corrected_sample, "before any correction").await;
    assert_eq!(
        sample["n"], 2,
        "both replicates count towards the sample: {sample}"
    );
    assert!(
        (f64_at(&sample, "mean") - 11.0).abs() < 1e-9,
        "the sample mean is the mean of 10.0 and 12.0: {sample}"
    );

    // The correction differs from the stored row in raw_value alone: it repeats the classification
    // and omits only the sample link, which a client correcting a value has no reason to resend.
    let (status, corrected) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/batch",
        &json!({
            "conflict": "overwrite",
            "readings": [replicate(corrected_at, 0, 20.0, None)],
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "correct one replicate ({status}): {corrected}");
    assert_eq!(
        corrected["overwritten"], 1,
        "the correction replaces the stored row rather than adding one: {corrected}"
    );

    let sample = sample_row(
        &app,
        &admin,
        &corrected_sample,
        "after correcting one replicate",
    )
    .await;
    assert_eq!(
        sample["n"], 2,
        "correcting a value must not unlink the replicate from its sample: {sample}"
    );
    assert!(
        (f64_at(&sample, "mean") - 16.0).abs() < 1e-9,
        "the sample mean follows the corrected value, ie. the mean of 20.0 and 12.0: {sample}"
    );
    assert_eq!(
        sample["label"], "morning plate",
        "the operator-entered label survives a value correction: {sample}"
    );
    assert_eq!(
        sample["notes"], "replicate plate",
        "the operator-entered notes survive a value correction: {sample}"
    );

    let (status, corrected) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/batch",
        &json!({
            "conflict": "overwrite",
            "readings": [replicate(corrected_at, 1, 22.0, None)],
        }),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "correct the remaining replicate ({status}): {corrected}"
    );

    let sample = sample_row(
        &app,
        &admin,
        &corrected_sample,
        "after correcting every replicate",
    )
    .await;
    assert_eq!(
        sample["n"], 2,
        "correcting the last replicate must not leave the sample referenced by nothing: {sample}"
    );
    assert!(
        (f64_at(&sample, "mean") - 21.0).abs() < 1e-9,
        "the sample mean is the mean of 20.0 and 22.0: {sample}"
    );

    // A correction that does repeat the link, and the plate nobody touched, must both be unmoved.
    let (status, corrected) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/batch",
        &json!({
            "conflict": "overwrite",
            "readings": [replicate(untouched_at, 0, 20.0, Some(untouched_sample.as_str()))],
        }),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "correct with the link repeated ({status}): {corrected}"
    );

    let sample = sample_row(
        &app,
        &admin,
        &untouched_sample,
        "the explicitly relinked plate",
    )
    .await;
    assert_eq!(
        sample["n"], 2,
        "the relinked correction keeps both replicates: {sample}"
    );
    assert!(
        (f64_at(&sample, "mean") - 16.0).abs() < 1e-9,
        "the relinked plate averages 20.0 and 12.0: {sample}"
    );
    assert_eq!(
        sample["label"], "midday plate",
        "the untouched label is unchanged: {sample}"
    );
}

/// a CSV import in `overwrite` mode must replace the value already stored for the slot,
/// even when that value arrived on a sync stream, rather than adding a second reading beside it.
#[tokio::test]
#[serial]
async fn csv_overwrite_replaces_a_synced_reading_instead_of_duplicating_the_slot() {
    if !kc::require_keycloak_or_skip("csv_overwrite_replaces_a_synced_reading").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let slot = provision_slot(&app, &admin, "csvovw", "IngCsvOvwTurb").await;
    let stream_id =
        register_and_pair(&app, &admin, "vaisala", "loc-1270", &slot.site_parameter_id).await;

    let at = "2025-06-11T09:00:00Z";
    let (status, ingested) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &json!({
            "stream_id": stream_id,
            "readings": [{ "time": at, "raw_value": 15.0 }],
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "sync ingest ({status}): {ingested}");
    assert_eq!(
        ingested["inserted"], 1,
        "the synced reading lands: {ingested}"
    );

    let imported = import_csv(
        &app,
        &admin,
        &json!({
            "site": slot.site_id,
            "csv": "DateTime,IngCsvOvwTurb\n2025-06-11 09:00:00,510.0\n",
            "dry_run": false,
            "conflict": "overwrite",
        }),
    )
    .await;
    assert_eq!(
        imported["overlaps_differing"], 1,
        "the import sees the stored 15.0 as a differing overlap: {imported}"
    );
    assert_eq!(
        imported["overwritten"], 1,
        "the operator is told one stored reading was replaced: {imported}"
    );
    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "csv_import", 30).await,
        "the csv_import job runs and succeeds"
    );

    let stored = e2e::count(
        &db,
        &format!(
            "SELECT count(*) FROM readings WHERE site_id = '{}' AND parameter_id = '{}' \
             AND time = '{at}'",
            slot.site_id, slot.parameter_id
        ),
    )
    .await;
    assert_eq!(
        stored, 1,
        "an overwrite leaves one reading in the slot, not the old one plus a second on the \
         import's own stream"
    );

    let superseded = e2e::count(
        &db,
        &format!(
            "SELECT count(*) FROM readings WHERE site_id = '{}' AND parameter_id = '{}' \
             AND time = '{at}' AND raw_value = 15.0",
            slot.site_id, slot.parameter_id
        ),
    )
    .await;
    assert_eq!(superseded, 0, "the value the operator replaced is gone");

    e2e::refresh_hourly(&db, instant("2025-06-11T00:00:00Z")).await;
    let (mean, count) = bucket(
        &db,
        &slot.site_id,
        &slot.parameter_id,
        instant(at),
        "after the overwrite",
    )
    .await;
    assert_eq!(
        count, 1,
        "the hourly rollup counts the slot once, ie. the overwrite does not double count"
    );
    assert!(
        (mean - 510.0).abs() < 1e-9,
        "the rollup carries the imported value, not the mean of old and new: got {mean}"
    );
}

/// `/ingest` must reject a timestamp outside `[now - 10 years, now + 1 day]` the way
/// `/readings/batch` already does, so a bad upstream timestamp cannot latch the stream cursor.
#[tokio::test]
#[serial]
async fn ingest_refuses_timestamps_outside_the_window_batch_already_enforces() {
    if !kc::require_keycloak_or_skip("ingest_refuses_timestamps_outside_the_window").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let slot = provision_slot(&app, &admin, "bounds", "IngBoundsDepth").await;
    let stream_id =
        register_and_pair(&app, &admin, "vaisala", "loc-1248", &slot.site_parameter_id).await;

    let now = chrono::Utc::now();
    let far_future = seconds_rfc3339(now + chrono::Duration::days(30));
    let far_past = seconds_rfc3339(now - chrono::Duration::days(365 * 11));

    // The sibling write path is the source of intent: it refuses both ends of the same window.
    for (label, at) in [("future", &far_future), ("past", &far_past)] {
        let (status, refused) = crate::common::post_json_with_token(
            &app,
            "/api/readings/batch",
            &json!({
                "readings": [{
                    "site_id": slot.site_id,
                    "parameter_id": slot.parameter_id,
                    "time": at,
                    "raw_value": 42.0,
                }]
            }),
            &admin,
        )
        .await;
        assert_eq!(
            status, 400,
            "/readings/batch refuses a {label} timestamp ({status}): {refused}"
        );
    }

    for (label, at) in [("future", &far_future), ("past", &far_past)] {
        let (status, accepted) = crate::common::post_json_with_token(
            &app,
            "/api/ingest",
            &json!({
                "stream_id": stream_id,
                "readings": [{ "time": at, "raw_value": 42.0 }],
            }),
            &admin,
        )
        .await;
        assert_eq!(
            status, 400,
            "/ingest refuses a {label} timestamp on the same window as /readings/batch \
             ({status}): {accepted}"
        );
    }

    let out_of_range = e2e::count(
        &db,
        &format!("SELECT count(*) FROM readings WHERE stream_id = '{stream_id}'"),
    )
    .await;
    assert_eq!(out_of_range, 0, "a refused ingest stores nothing");

    let (status, stream) =
        crate::common::get_json_with_token(&app, &format!("/api/data_streams/{stream_id}"), &admin)
            .await;
    assert_eq!(status, 200, "read the stream back ({status}): {stream}");
    assert!(
        stream["last_data_time"].is_null(),
        "a refused ingest must not advance the incremental-sync cursor: {stream}"
    );

    // The bound rejects only what falls outside it: a routine ingest still lands and still moves
    // the cursor.
    let in_range = seconds_rfc3339(now - chrono::Duration::hours(1));
    let (status, ingested) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &json!({
            "stream_id": stream_id,
            "readings": [{ "time": in_range, "raw_value": 42.0 }],
        }),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "an in-range ingest is accepted ({status}): {ingested}"
    );
    assert_eq!(
        ingested["inserted"], 1,
        "the in-range reading lands: {ingested}"
    );

    let (status, stream) =
        crate::common::get_json_with_token(&app, &format!("/api/data_streams/{stream_id}"), &admin)
            .await;
    assert_eq!(status, 200, "read the stream back ({status}): {stream}");
    let cursor = stream["last_data_time"]
        .as_str()
        .unwrap_or_else(|| panic!("an accepted ingest sets the cursor: {stream}"));
    assert_eq!(
        instant(cursor),
        instant(&in_range),
        "the cursor tracks the accepted reading: {stream}"
    );
}

/// a non-finite CSV cell is a row error, not a stored measurement, so one bad cell cannot
/// blank the aggregate bucket it falls in.
#[tokio::test]
#[serial]
async fn csv_import_refuses_a_non_finite_cell_and_leaves_the_bucket_computable() {
    if !kc::require_keycloak_or_skip("csv_import_refuses_a_non_finite_cell").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let slot = provision_slot(&app, &admin, "inf", "IngNonFinite").await;

    // Twelve five-minute rows inside one hour: ten measurements, one `Inf` (the spelling R's
    // `write.csv` emits) and one `NaN` (the declared missing-value marker).
    let mut csv = String::from("DateTime,IngNonFinite\n");
    for i in 0..12 {
        let cell = match i {
            6 => "Inf",
            11 => "NaN",
            _ => "100.0",
        };
        csv.push_str(&format!("2025-06-12 09:{:02}:00,{cell}\n", i * 5));
    }

    let imported = import_csv(
        &app,
        &admin,
        &json!({ "site": slot.site_id, "csv": csv, "dry_run": false }),
    )
    .await;
    assert_eq!(
        imported["error_count"], 1,
        "the `Inf` cell is reported as a row error and the `NaN` cell is not: {imported}"
    );
    let errors = imported["errors"]
        .as_array()
        .unwrap_or_else(|| panic!("the import reports an errors array: {imported}"));
    assert_eq!(errors.len(), 1, "exactly one cell is rejected: {imported}");
    assert_eq!(
        errors[0]["row"], 8,
        "the reported line is the one carrying `Inf`, the header being line 1: {imported}"
    );

    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "csv_import", 30).await,
        "the csv_import job runs and succeeds"
    );

    let non_finite = e2e::count(
        &db,
        &format!(
            "SELECT count(*) FROM readings WHERE site_id = '{}' \
             AND raw_value IN ('Infinity'::float8, '-Infinity'::float8, 'NaN'::float8)",
            slot.site_id
        ),
    )
    .await;
    assert_eq!(
        non_finite, 0,
        "no non-finite value reaches the readings table"
    );

    let stored = e2e::count(
        &db,
        &format!(
            "SELECT count(*) FROM readings WHERE site_id = '{}' AND parameter_id = '{}'",
            slot.site_id, slot.parameter_id
        ),
    )
    .await;
    assert_eq!(
        stored, 10,
        "the ten measurements import; the `Inf` cell is rejected and the `NaN` cell is a missing \
         value"
    );

    e2e::refresh_hourly(&db, instant("2025-06-12T00:00:00Z")).await;
    let (mean, count) = bucket(
        &db,
        &slot.site_id,
        &slot.parameter_id,
        instant("2025-06-12T09:00:00Z"),
        "after importing the file",
    )
    .await;
    assert_eq!(count, 10, "the bucket counts the ten measurements");
    assert!(
        (mean - 100.0).abs() < 1e-9,
        "one bad cell must not blank the bucket mean: got {mean}"
    );
}

/// the `-9999` missing-value sentinel is recognised by value, so its decimal spellings are
/// skipped like the bare one rather than averaged in as measurements.
#[tokio::test]
#[serial]
async fn csv_import_treats_every_spelling_of_the_sentinel_as_missing() {
    if !kc::require_keycloak_or_skip("csv_import_treats_every_spelling_of_the_sentinel").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let decimal_slot = provision_slot(&app, &admin, "sent", "IngSentDecimal").await;
    let bare_parameter_id =
        e2e::create_parameter(&app, &admin, "IngSentBare", "Ingest sentinel bare", "ppb").await;
    e2e::assign_site_parameter_minimal(&app, &admin, &decimal_slot.site_id, &bare_parameter_id)
        .await;

    // Five measurements then two sentinel rows, inside one hour. The two columns differ only in
    // how the source spells the sentinel.
    let mut csv = String::from("DateTime,IngSentDecimal,IngSentBare\n");
    for i in 0..7 {
        let (decimal, bare) = match i {
            5 => ("-9999.0", "-9999"),
            6 => ("-9999.00", "-9999"),
            _ => ("1000.0", "1000.0"),
        };
        csv.push_str(&format!("2025-06-13 09:{:02}:00,{decimal},{bare}\n", i * 8));
    }

    let imported = import_csv(
        &app,
        &admin,
        &json!({ "site": decimal_slot.site_id, "csv": csv, "dry_run": false }),
    )
    .await;
    assert_eq!(
        imported["error_count"], 0,
        "a sentinel is a declared missing value, not a malformed cell: {imported}"
    );
    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "csv_import", 30).await,
        "the csv_import job runs and succeeds"
    );

    let negative = e2e::count(
        &db,
        &format!(
            "SELECT count(*) FROM readings WHERE site_id = '{}' AND raw_value < 0",
            decimal_slot.site_id
        ),
    )
    .await;
    assert_eq!(negative, 0, "no sentinel is stored as a measurement");

    e2e::refresh_hourly(&db, instant("2025-06-13T00:00:00Z")).await;
    let at = instant("2025-06-13T09:00:00Z");

    let (mean, count) = bucket(
        &db,
        &decimal_slot.site_id,
        &decimal_slot.parameter_id,
        at,
        "the decimal-spelled sentinel column",
    )
    .await;
    assert_eq!(count, 5, "only the five measurements are counted");
    assert!(
        (mean - 1000.0).abs() < 1e-9,
        "`-9999.0` and `-9999.00` are missing values, so the mean is that of the measurements: \
         got {mean}"
    );

    let (mean, count) = bucket(
        &db,
        &decimal_slot.site_id,
        &bare_parameter_id,
        at,
        "the bare-spelled sentinel column",
    )
    .await;
    assert_eq!(count, 5, "the bare spelling is skipped, as it already was");
    assert!(
        (mean - 1000.0).abs() < 1e-9,
        "the recognised spelling keeps the column's mean at the measurements: got {mean}"
    );
}

/// a timestamp repeated in a continuous-cadence import is a row error, not a hidden
/// replicate and not a fabricated grab sample.
#[tokio::test]
#[serial]
async fn csv_import_refuses_a_duplicated_timestamp_in_a_continuous_file() {
    if !kc::require_keycloak_or_skip("csv_import_refuses_a_duplicated_timestamp").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let continuous_slot = provision_slot(&app, &admin, "dup", "IngDupContinuous").await;
    let spot_parameter_id =
        e2e::create_parameter(&app, &admin, "IngDupSpot", "Ingest duplicate spot", "uM").await;
    e2e::assign_site_parameter_minimal(&app, &admin, &continuous_slot.site_id, &spot_parameter_id)
        .await;

    let value_row = DUPLICATE_TIMESTAMPS
        .lines()
        .find(|line| line.starts_with(FIXTURE_VALUE_ROW))
        .unwrap_or_else(|| panic!("the fixture carries a {FIXTURE_VALUE_ROW} row"));
    let repeated_value: f64 = value_row
        .split(',')
        .nth(2)
        .and_then(|cell| cell.parse().ok())
        .unwrap_or_else(|| panic!("the {FIXTURE_VALUE_ROW} row carries a DOuM value: {value_row}"));
    let mut csv = String::new();
    for line in DUPLICATE_TIMESTAMPS.lines() {
        csv.push_str(line);
        csv.push('\n');
        if line == value_row {
            csv.push_str(line);
            csv.push('\n');
        }
    }

    let imported = import_csv(
        &app,
        &admin,
        &json!({
            "site": continuous_slot.site_id,
            "csv": csv,
            "dry_run": false,
            "measurement_type": "continuous",
            "mapping": {
                "DOmgL": null,
                "DOuM": continuous_slot.parameter_id,
                "WaterTempdegC": null,
            },
        }),
    )
    .await;
    assert!(
        imported["error_count"].as_u64().unwrap_or(0) >= 1,
        "the repeated timestamp is reported rather than silently absorbed: {imported}"
    );
    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "csv_import", 30).await,
        "the csv_import job runs and succeeds"
    );

    let at = format!("{FIXTURE_VALUE_ROW}+00");
    let at_repeat = e2e::count(
        &db,
        &format!(
            "SELECT count(*) FROM readings WHERE site_id = '{}' AND parameter_id = '{}' \
             AND time = '{at}'",
            continuous_slot.site_id, continuous_slot.parameter_id
        ),
    )
    .await;
    assert_eq!(
        at_repeat, 1,
        "a continuous cadence holds one reading per timestamp"
    );

    let hidden = e2e::count(
        &db,
        &format!(
            "SELECT count(*) FROM readings WHERE site_id = '{}' AND parameter_id = '{}' \
             AND replicate_index > 0",
            continuous_slot.site_id, continuous_slot.parameter_id
        ),
    )
    .await;
    assert_eq!(
        hidden, 0,
        "a continuous import creates no replicate that the default read and the rollups filter out"
    );

    let fabricated = e2e::count(
        &db,
        &format!(
            "SELECT count(*) FROM samples WHERE site_id = '{}' AND parameter_id = '{}'",
            continuous_slot.site_id, continuous_slot.parameter_id
        ),
    )
    .await;
    assert_eq!(
        fabricated, 0,
        "a duplicated timestamp in a continuous file is not a grab sample"
    );

    // The same file declared as spot data is a genuine replicate group and must still become one.
    import_csv(
        &app,
        &admin,
        &json!({
            "site": continuous_slot.site_id,
            "csv": csv,
            "dry_run": false,
            "measurement_type": "spot",
            "mapping": {
                "DOmgL": null,
                "DOuM": spot_parameter_id,
                "WaterTempdegC": null,
            },
        }),
    )
    .await;
    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "csv_import", 30).await,
        "the second csv_import job runs and succeeds"
    );

    let filter = e2e::percent_encode(&format!(r#"{{"parameter_id":"{spot_parameter_id}"}}"#));
    let (status, samples) =
        crate::common::get_json_with_token(&app, &format!("/api/samples?filter={filter}"), &admin)
            .await;
    assert_eq!(
        status, 200,
        "list the spot column's samples ({status}): {samples}"
    );
    let rows = samples
        .as_array()
        .unwrap_or_else(|| panic!("the samples list is an array: {samples}"));
    assert_eq!(
        rows.len(),
        1,
        "the spot-declared replicate group is one sample: {samples}"
    );
    assert_eq!(rows[0]["n"], 2, "both replicates count: {samples}");
    assert!(
        (f64_at(&rows[0], "mean") - repeated_value).abs() < 1e-9,
        "the sample mean is the repeated value: {samples}"
    );
}

/// a grab collected without replicates is still a sample, so it appears in the
/// sensor-vs-grab comparison alongside grabs that happen to carry two.
#[tokio::test]
#[serial]
async fn a_single_replicate_grab_reaches_the_sensor_vs_grab_export() {
    if !kc::require_keycloak_or_skip("a_single_replicate_grab_reaches_the_export").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_grab_track(&app, &admin).await;
    let parameter_id = track.parameter_id("TrkGrabDoc").to_string();
    let day = "2025-06-14";

    let lone_grab_at = format!("{day}T06:00:00Z");
    let (status, lone) = crate::common::post_json_parse_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": track.site_id,
            "readings": [{
                "parameter_id": parameter_id,
                "time": lone_grab_at,
                "value": 310.0,
            }],
        }),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "single-replicate grab entry ({status}): {lone}"
    );
    assert_eq!(lone["inserted"], 1, "the grab reading lands: {lone}");
    assert_eq!(
        lone["samples_created"], 1,
        "a grab is a sample whether or not it was measured twice: {lone}"
    );

    let paired_grab_at = format!("{day}T14:00:00Z");
    let (status, pair) = crate::common::post_json_parse_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": track.site_id,
            "readings": tracks::grab_replicates(&parameter_id, &paired_grab_at, &[320.0, 322.0]),
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "two-replicate grab entry ({status}): {pair}");
    assert_eq!(pair["samples_created"], 1, "the pair is one sample: {pair}");

    // Continuous points inside each grab's [T+2h, T+6h] window and outside the other's.
    let continuous = |hhmm: &str, value: f64| {
        json!({
            "site_id": track.site_id,
            "parameter_id": parameter_id,
            "time": format!("{day}T{hhmm}:00Z"),
            "raw_value": value,
            "measurement_type": "continuous",
        })
    };
    let (status, batch) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/batch",
        &json!({
            "readings": [
                continuous("09:00", 312.0),
                continuous("11:00", 314.0),
                continuous("17:00", 330.0),
            ]
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "continuous batch ({status}): {batch}");
    assert_eq!(
        batch["inserted"], 3,
        "all three continuous points land: {batch}"
    );

    let lone_sample = e2e::count(
        &db,
        &format!(
            "SELECT count(*) FROM samples WHERE site_id = '{}' AND parameter_id = '{parameter_id}' \
             AND collected_at = '{lone_grab_at}' AND n = 1",
            track.site_id
        ),
    )
    .await;
    assert_eq!(
        lone_sample, 1,
        "the lone grab has a samples row counting its one replicate"
    );

    let (status, export) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/sites/{}/export/sensor-vs-grab?parameter_id={parameter_id}\
             &start={day}T00:00:00Z&end={day}T23:59:59Z",
            track.site_id
        ),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "sensor-vs-grab export ({status}): {export}");
    let rows = export["rows"]
        .as_array()
        .unwrap_or_else(|| panic!("the export returns a rows array: {export}"));
    assert_eq!(
        rows.len(),
        2,
        "both grabs are compared, the single-replicate one included: {export}"
    );

    let lone_row = &rows[0];
    assert_eq!(
        lone_row["grab_n"], 1,
        "the lone grab reports one replicate: {lone_row}"
    );
    assert!(
        (f64_at(lone_row, "grab_value") - 310.0).abs() < 1e-9,
        "the lone grab's value is the measurement itself: {lone_row}"
    );
    assert_eq!(
        lone_row["sensor_n"], 2,
        "only the 09:00 and 11:00 points fall in its window: {lone_row}"
    );
    assert!(
        (f64_at(lone_row, "sensor_avg") - 313.0).abs() < 1e-9,
        "the sensor side averages 312.0 and 314.0: {lone_row}"
    );
    assert!(
        (f64_at(lone_row, "difference") + 3.0).abs() < 1e-9,
        "difference is grab minus sensor: {lone_row}"
    );

    let paired_row = &rows[1];
    assert_eq!(
        paired_row["grab_n"], 2,
        "the replicate pair is unchanged: {paired_row}"
    );
    assert!(
        (f64_at(paired_row, "grab_value") - 321.0).abs() < 1e-9,
        "the pair's value is its replicate mean: {paired_row}"
    );
    assert_eq!(
        paired_row["sensor_n"], 1,
        "only the 17:00 point falls in the pair's window: {paired_row}"
    );
    assert!(
        (f64_at(paired_row, "sensor_avg") - 330.0).abs() < 1e-9,
        "the pair's sensor side is the 17:00 point: {paired_row}"
    );
}

/// a CSV timestamp that already carries an offset is a resolved instant, so the
/// request-level `tz_offset_hours` must not shift it a second time.
#[tokio::test]
#[serial]
async fn csv_import_does_not_shift_a_timestamp_that_carries_its_own_offset() {
    if !kc::require_keycloak_or_skip("csv_import_does_not_shift_an_offset_timestamp").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let slot = provision_slot(&app, &admin, "tz", "IngTzDepth").await;
    let expected = instant("2025-06-15T10:00:00Z");

    let offset_preview = import_csv(
        &app,
        &admin,
        &json!({
            "site": slot.site_id,
            "csv": "DateTime,IngTzDepth\n2025-06-15T12:00:00+02:00,120.0\n",
            "dry_run": true,
            "tz_offset_hours": 2.0,
        }),
    )
    .await;
    assert_eq!(
        offset_preview["row_count"], 1,
        "the row parses: {offset_preview}"
    );
    let earliest = offset_preview["earliest"]
        .as_str()
        .unwrap_or_else(|| panic!("the preview reports the earliest instant: {offset_preview}"));
    assert_eq!(
        instant(earliest),
        expected,
        "12:00+02:00 is 10:00Z whichever zone the operator picked for the file: {offset_preview}"
    );

    // The same wall-clock reading written without an offset does need the picked zone applied.
    let naive_preview = import_csv(
        &app,
        &admin,
        &json!({
            "site": slot.site_id,
            "csv": "DateTime,IngTzDepth\n2025-06-15 12:00:00,120.0\n",
            "dry_run": true,
            "tz_offset_hours": 2.0,
        }),
    )
    .await;
    let earliest = naive_preview["earliest"]
        .as_str()
        .unwrap_or_else(|| panic!("the preview reports the earliest instant: {naive_preview}"));
    assert_eq!(
        instant(earliest),
        expected,
        "a naive 12:00 in UTC+02:00 is 10:00Z: {naive_preview}"
    );
}
