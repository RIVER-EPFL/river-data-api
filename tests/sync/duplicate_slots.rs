//! Serving returns one row per (site, parameter, instant), so a slot fed by two streams looks the
//! same on the chart as one fed by one. The reconciliation surface lists those slots instead.
//!
//! Run: cargo test --test sync duplicate_slots -- --test-threads=1

use serde_json::json;
use serial_test::serial;

const T1: &str = "2025-06-01T08:00:00Z";
const T2: &str = "2025-06-02T08:00:00Z";

struct Fixture {
    app: axum::Router,
    token: String,
}

async fn register_paired(fx: &Fixture, key: &str, site_parameter_id: &str) -> String {
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/streams/register",
        &json!({"source_system": "dupsrc", "source_key": key, "measurement_type": "spot"}),
        &fx.token,
    )
    .await;
    assert!((200..300).contains(&status), "register: {body}");
    let stream = crate::common::e2e::id_of(&body);
    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        &format!("/api/streams/{stream}/pair"),
        &json!({"site_parameter_id": site_parameter_id}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "pair ({status}): {body}");
    stream
}

async fn ingest(fx: &Fixture, stream: &str, times: &[&str]) {
    let readings: Vec<serde_json::Value> = times
        .iter()
        .map(|t| json!({"time": t, "raw_value": 1.5, "replicate_index": 0}))
        .collect();
    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        "/api/ingest",
        &json!({"stream_id": stream, "readings": readings}),
        &fx.token,
    )
    .await;
    assert!((200..300).contains(&status), "ingest ({status}): {body}");
}

async fn setup() -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    Fixture { app, token }
}

async fn list(fx: &Fixture) -> serde_json::Value {
    let (status, body) = crate::common::get_json_with_token(
        &fx.app,
        "/api/sync/replicate_reconciliation/duplicate_slots",
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "duplicate slots ({status}): {body}");
    body
}

#[tokio::test]
#[serial]
async fn a_slot_one_stream_feeds_is_not_listed() {
    let fx = setup().await;
    let stream = register_paired(&fx, "solo:reps", crate::common::PARAM_S1_TEMP_ID).await;
    ingest(&fx, &stream, &[T1, T2]).await;

    let body = list(&fx).await;
    assert_eq!(
        body["slots"].as_array().expect("slots array").len(),
        0,
        "one stream per slot is the normal case: {body}"
    );
}

#[tokio::test]
#[serial]
async fn a_slot_two_streams_feed_is_listed_with_both_and_the_instants_they_share() {
    let fx = setup().await;
    let legacy = register_paired(&fx, "temp_avg", crate::common::PARAM_S1_TEMP_ID).await;
    let family = register_paired(&fx, "temp_avg:reps", crate::common::PARAM_S1_TEMP_ID).await;
    // The shared instant is the duplicate; the second is fed by the family stream alone.
    ingest(&fx, &legacy, &[T1]).await;
    ingest(&fx, &family, &[T1, T2]).await;
    // A neighbouring slot with one stream must not be dragged in.
    let other = register_paired(&fx, "do_avg:reps", crate::common::PARAM_S1_DO_ID).await;
    ingest(&fx, &other, &[T1]).await;

    let body = list(&fx).await;
    let slots = body["slots"].as_array().expect("slots array");
    assert_eq!(slots.len(), 1, "only the shared slot is listed: {body}");
    let slot = &slots[0];
    assert_eq!(slot["site_parameter_id"], crate::common::PARAM_S1_TEMP_ID);
    assert_eq!(slot["site_name"], "Upstream Station");
    assert_eq!(
        slot["duplicated_instants"], 1,
        "one instant carries both streams"
    );

    // Every feed of the slot is named, the seeded sensor stream included: the operator is deciding
    // which of them keeps the slot.
    let streams = slot["streams"].as_array().expect("streams array");
    assert_eq!(streams.len(), 3);
    let ids: Vec<&str> = streams
        .iter()
        .map(|s| s["stream_id"].as_str().expect("stream id"))
        .collect();
    assert!(
        ids.contains(&legacy.as_str()) && ids.contains(&family.as_str()),
        "both feeds named"
    );
    let fam = streams
        .iter()
        .find(|s| s["stream_id"] == family.as_str())
        .expect("family stream listed");
    assert_eq!(fam["readings"], 2, "each feed reports its own volume");
    assert_eq!(fam["first_reading"].as_str().is_some(), true);
}

#[tokio::test]
#[serial]
async fn an_unpaired_second_stream_does_not_duplicate_the_slot() {
    let fx = setup().await;
    let family = register_paired(&fx, "cond_avg:reps", crate::common::PARAM_S1_COND_ID).await;
    ingest(&fx, &family, &[T1]).await;
    // Registered against the same source but never paired: its readings carry no slot at all.
    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        "/api/streams/register",
        &json!({"source_system": "dupsrc", "source_key": "cond_avg", "measurement_type": "spot"}),
        &fx.token,
    )
    .await;
    assert!((200..300).contains(&status), "register ({status}): {body}");

    let body = list(&fx).await;
    assert_eq!(
        body["slots"].as_array().expect("slots array").len(),
        0,
        "attribution comes from the pairing, so an unpaired stream feeds no slot: {body}"
    );
}
