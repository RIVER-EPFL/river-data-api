//! `GET /actions/duplicate_slots`: the same (site, parameter, instant) served by more than one
//! stream, which every write path accepts because deduplication is keyed per stream while the
//! rollups group per slot.
//!
//! Run: cargo test --test readings duplicate_slots -- --test-threads=1

use crate::common::e2e;
use serial_test::serial;

const TIMES: [&str; 3] = [
    "2025-02-10T00:00:00Z",
    "2025-02-10T01:00:00Z",
    "2025-02-10T02:00:00Z",
];

struct Slot {
    db: sea_orm::DatabaseConnection,
    app: axum::Router,
    token: String,
    project: String,
    site: String,
    parameter: String,
    synced_stream: String,
}

/// A slot fed by a paired sync stream, holding one reading per instant in [`TIMES`].
async fn slot_fed_by_one_stream(key: &str) -> Slot {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let project = e2e::create_project(&app, &token, "Dup P", "dup-p", false).await;
    let site = e2e::create_site(&app, &token, &project, "Dup Site", "dup-site").await;
    let parameter = e2e::create_parameter(&app, &token, "dupcond", "Conductivity", "uS/cm").await;
    let sp = e2e::assign_site_parameter_minimal(&app, &token, &site, &parameter).await;

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &serde_json::json!({"source_system": "portal", "source_key": key}),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "register ({status}): {stream}"
    );
    let synced_stream = e2e::id_of(&stream);

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{synced_stream}/pair"),
        &serde_json::json!({"site_parameter_id": sp}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "pair ({status}): {body}");

    let readings: Vec<serde_json::Value> = TIMES
        .iter()
        .enumerate()
        .map(|(i, t)| serde_json::json!({"time": t, "raw_value": 100.0 + i as f64}))
        .collect();
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &serde_json::json!({"stream_id": synced_stream, "readings": readings}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "ingest ({status}): {body}");

    Slot {
        db,
        app,
        token,
        project,
        site,
        parameter,
        synced_stream,
    }
}

/// The report for one slot, or `None` when the slot is absent from it.
async fn report_for(slot: &Slot) -> (serde_json::Value, Option<serde_json::Value>) {
    let (status, body) = crate::common::get_with_token(
        &slot.app,
        "/api/actions/duplicate_slots?since=2025-01-01T00:00:00Z",
        &slot.token,
    )
    .await;
    assert_eq!(status, 200, "duplicate_slots ({status}): {body}");
    let report: serde_json::Value = serde_json::from_str(&body).expect("report json");
    let found = report["slots"]
        .as_array()
        .expect("slots array")
        .iter()
        .find(|s| s["site_id"] == slot.site && s["parameter_id"] == slot.parameter)
        .cloned();
    (report, found)
}

/// A second channel writing the same instants through `/readings/batch`, which pairs its own `api`
/// stream to the slot the request names. This is how the shape arises in practice: a historical
/// import lands alongside a running sync.
async fn write_second_copy(slot: &Slot, values: [f64; 3]) {
    let readings: Vec<serde_json::Value> = TIMES
        .iter()
        .zip(values)
        .map(|(t, v)| {
            serde_json::json!({
                "site_id": slot.site, "parameter_id": slot.parameter,
                "time": t, "raw_value": v,
            })
        })
        .collect();
    let (status, body) = crate::common::post_json_parse_with_token(
        &slot.app,
        "/api/readings/batch",
        &serde_json::json!({ "readings": readings }),
        &slot.token,
    )
    .await;
    assert_eq!(status, 200, "second copy ({status}): {body}");
}

#[tokio::test]
#[serial]
async fn a_slot_fed_by_two_streams_is_reported_with_the_size_of_the_disagreement() {
    let slot = slot_fed_by_one_stream("dup-two").await;
    write_second_copy(&slot, [100.0, 105.0, 110.0]).await;

    assert_eq!(
        e2e::count(
            &slot.db,
            &format!(
                "SELECT count(*) AS c FROM readings WHERE site_id = '{}' AND parameter_id = '{}'",
                slot.site, slot.parameter
            )
        )
        .await,
        6,
        "both copies are stored: the primary key is per stream"
    );

    let (report, found) = report_for(&slot).await;
    let found = found.expect("the slot is reported");

    assert_eq!(found["overlapping_instants"], 3, "{found}");
    assert_eq!(
        found["disagreeing_instants"], 2,
        "the instant where both copies read 100 is redundant, not contradictory: {found}"
    );
    let max = found["max_difference"].as_f64().expect("max_difference");
    let mean = found["mean_difference"].as_f64().expect("mean_difference");
    assert!(
        (max - 8.0).abs() < 1e-9,
        "spread of 110 against 102: {found}"
    );
    assert!(
        (mean - 6.0).abs() < 1e-9,
        "mean over the disagreeing instants alone, (4 + 8) / 2: {found}"
    );

    let streams = found["streams"].as_array().expect("streams");
    assert_eq!(streams.len(), 2, "both channels are named: {found}");
    assert!(
        streams
            .iter()
            .all(|s| s["instants"].as_i64() == Some(3)
                && !s["source_key"].as_str().unwrap().is_empty()),
        "each channel contributes to every overlapping instant: {found}"
    );
    let systems: Vec<&str> = streams
        .iter()
        .map(|s| s["source_system"].as_str().unwrap())
        .collect();
    assert!(
        systems.contains(&"portal") && systems.contains(&"api"),
        "the sync channel and the import channel: {found}"
    );

    assert_eq!(report["total_overlapping_instants"], 3, "{report}");
    assert_eq!(report["total_disagreeing_instants"], 2, "{report}");
    assert_eq!(
        report["scanned_from"], "2025-01-01T00:00:00Z",
        "the floor that was read comes back: {report}"
    );
}

#[tokio::test]
#[serial]
async fn one_stream_per_instant_is_not_a_duplicate() {
    let slot = slot_fed_by_one_stream("dup-one").await;

    let (report, found) = report_for(&slot).await;
    assert!(
        found.is_none(),
        "a slot with one channel is absent from the report: {report}"
    );
    assert_eq!(report["total_slots"], 0, "{report}");
}

/// The report enumerates the site and parameter of every slot it finds, so a token holding one
/// project cannot read it at all: the enumeration is the thing that must not leak.
#[tokio::test]
#[serial]
async fn a_project_scoped_token_cannot_read_the_report() {
    let slot = slot_fed_by_one_stream("dup-scope").await;
    let scoped = crate::common::seed_api_token(
        &slot.db,
        crate::common::perms(true, true, false, false),
        Some(&slot.project),
    )
    .await;

    let (status, body) =
        crate::common::get_with_token(&slot.app, "/api/actions/duplicate_slots", &scoped).await;
    assert_eq!(status, 403, "a scoped token is refused ({status}): {body}");
}

/// A replicate family's indexes are the source's column positions and need not align with
/// another stream's, so overlap is detected on what each stream would serve at an instant (the
/// mean over its unflagged replicates), never per index. Grouping by index would silently miss
/// exactly this shape: the family holds indexes 1 and 2, the other channel index 0.
#[tokio::test]
#[serial]
async fn a_replicate_family_overlapping_another_stream_is_reported() {
    let slot = slot_fed_by_one_stream("dup-family").await;

    let family = uuid::Uuid::new_v4();
    crate::common::exec(
        &slot.db,
        &format!(
            "INSERT INTO data_streams \
                (id, source_system, source_key, is_active, measurement_type, metadata) \
             VALUES ('{family}', 'portal', 'DUP:cond_avg:reps', true, 'spot', \
                     '{{\"replicates\": {{\"source_columns\": [\"cond_1\", \"cond_2\"]}}}}')"
        ),
    )
    .await;
    // TIMES[0]: family mean 100.0 agrees with the synced 100.0 (redundant).
    // TIMES[1]: family mean 104.0 against the synced 101.0 (disagreeing by 3).
    for (time, index, value) in [
        (TIMES[0], 1, 99.0),
        (TIMES[0], 2, 101.0),
        (TIMES[1], 1, 103.0),
        (TIMES[1], 2, 105.0),
    ] {
        crate::common::exec(
            &slot.db,
            &format!(
                "INSERT INTO readings \
                    (stream_id, site_id, parameter_id, time, replicate_index, raw_value, \
                     logged, measurement_type, is_flagged) \
                 VALUES ('{family}', '{site}', '{param}', '{time}', {index}, {value}, \
                         true, 'spot', false)",
                site = slot.site,
                param = slot.parameter,
            ),
        )
        .await;
    }

    let (_, found) = report_for(&slot).await;
    let found = found.expect("the family overlap is reported");
    assert_eq!(found["overlapping_instants"], 2, "{found}");
    assert_eq!(
        found["disagreeing_instants"], 1,
        "the instant where the family's served mean matches is redundant: {found}"
    );
    assert!(
        (found["max_difference"].as_f64().expect("max_difference") - 3.0).abs() < 1e-9,
        "spread between served values, not raw replicates: {found}"
    );
    let systems: Vec<&str> = found["streams"]
        .as_array()
        .expect("streams")
        .iter()
        .map(|s| s["source_key"].as_str().unwrap())
        .collect();
    assert!(systems.contains(&"DUP:cond_avg:reps"), "{found}");
}

/// A flagged copy is already out of every rollup, so an instant it leaves single-sourced is no
/// longer a disagreement anything averages over.
#[tokio::test]
#[serial]
async fn a_flagged_copy_is_not_a_second_population() {
    let slot = slot_fed_by_one_stream("dup-flag").await;
    write_second_copy(&slot, [100.0, 105.0, 110.0]).await;

    let (status, body) = crate::common::patch_json_with_token(
        &slot.app,
        "/api/readings/flag",
        &serde_json::json!({
            "reason": "duplicate import",
            "readings": [
                { "site_id": slot.site, "parameter_id": slot.parameter, "time": TIMES[2] },
            ],
        }),
        &slot.token,
    )
    .await;
    assert_eq!(status, 200, "flag ({status}): {body}");

    let (_, found) = report_for(&slot).await;
    let found = found.expect("the remaining instants are still reported");
    assert_eq!(
        found["overlapping_instants"], 2,
        "the flagged instant drops out entirely: {found}"
    );
    assert_eq!(found["disagreeing_instants"], 1, "{found}");
    assert!(
        (found["max_difference"].as_f64().expect("max_difference") - 4.0).abs() < 1e-9,
        "the widest surviving spread: {found}"
    );

    assert_eq!(
        e2e::count(
            &slot.db,
            &format!(
                "SELECT count(*) AS c FROM readings WHERE stream_id = '{}' AND is_flagged",
                slot.synced_stream
            )
        )
        .await,
        1,
        "flagging by slot reaches every copy at that instant"
    );
}
