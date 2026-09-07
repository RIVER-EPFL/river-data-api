//! Response-cache key completeness and invalidation.
//!
//! Every test runs against the cache-enabled builder (`cache_ttl_seconds = 300`). The default test
//! builder has the cache off, so the same request sequences would pass on it while proving nothing.
//! `X-Cache: HIT|MISS` is the instrument that says whether a body came out of the cache.
//!
//! A bounded window (an explicit `end`) is used wherever the subject is the cache KEY, because
//! `common/cache.rs:140` skips the freshness probe whenever `end` is supplied; the one unbounded
//! test is the one whose subject is the probe itself.
//!
//! These run as real Keycloak users, so they assert what a dashboard user is served, and self-skip
//! when Keycloak is unreachable unless `REQUIRE_KEYCLOAK` is set. What a key is *made of* is not
//! asserted here: `common/cache_key.rs` and the two `cache_key_tests` modules beside the handlers
//! cover that with no database.
//!
//! Run: cargo test --test cache -- --test-threads=1

use axum::Router;
use axum::body::Body;
use http_body_util::BodyExt;
use serde_json::json;
use serial_test::serial;
use tower::ServiceExt;

use crate::common::e2e;
use crate::common::keycloak as kc;
use crate::common::tracks;

/// One GET, with the cache verdict kept alongside the body.
struct Probe {
    status: u16,
    /// `X-Cache` header value, or "absent" on responses that carry none.
    cache: String,
    body: String,
    json: serde_json::Value,
}

async fn probe(app: &Router, uri: &str, jwt: Option<&str>) -> Probe {
    let mut builder = axum::http::Request::builder().method("GET").uri(uri);
    if let Some(token) = jwt {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let cache = response
        .headers()
        .get("X-Cache")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("absent")
        .to_string();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&bytes).to_string();
    let json = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    Probe {
        status,
        cache,
        body,
        json,
    }
}

struct Slot {
    project_id: String,
    site_id: String,
    parameter_id: String,
    site_parameter_id: String,
}

/// A public project holding one site with one parameter assigned, provisioned the way the
/// dashboard does it. `slug` keeps codes unique between the slots a single test provisions.
async fn provision_slot(app: &Router, jwt: &str, slug: &str) -> Slot {
    let project_id = e2e::create_project(
        app,
        jwt,
        &format!("Cache project {slug}"),
        &format!("cache-{slug}"),
        true,
    )
    .await;
    let site_id = e2e::create_site(
        app,
        jwt,
        &project_id,
        &format!("Cache site {slug}"),
        &format!("cache-site-{slug}"),
    )
    .await;
    let parameter_id = e2e::create_parameter(
        app,
        jwt,
        &format!("CacheDepth{slug}"),
        &format!("Cache depth {slug}"),
        "mm",
    )
    .await;
    let site_parameter_id =
        e2e::assign_site_parameter_minimal(app, jwt, &site_id, &parameter_id).await;
    Slot {
        project_id,
        site_id,
        parameter_id,
        site_parameter_id,
    }
}

/// `POST /readings/batch` with a single reading on a slot, asserting it landed.
async fn batch_one(app: &Router, jwt: &str, slot: &Slot, time: &str, value: f64) {
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/readings/batch",
        &json!({
            "readings": [{
                "site_id": slot.site_id,
                "parameter_id": slot.parameter_id,
                "time": time,
                "raw_value": value,
            }]
        }),
        jwt,
    )
    .await;
    assert_eq!(status, 200, "batch insert at {time} ({status}): {body}");
    assert_eq!(body["inserted"], 1, "one reading landed at {time}: {body}");
}

/// `POST /ingest` with a single reading on a paired stream, asserting it landed.
async fn ingest_one(app: &Router, jwt: &str, stream_id: &str, time: &str, raw_value: f64) {
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/ingest",
        &json!({ "stream_id": stream_id, "readings": [{ "time": time, "raw_value": raw_value }] }),
        jwt,
    )
    .await;
    assert_eq!(status, 200, "ingest at {time} ({status}): {body}");
    assert_eq!(body["inserted"], 1, "one reading landed at {time}: {body}");
    assert_eq!(
        body["paired"], true,
        "the stream is paired, so the reading is attributed: {body}"
    );
}

// cache::invalidate_prefix removes nothing (the moka cache is built without
// support_invalidation_closures), so a bounded read keeps serving pre-write bytes.
#[tokio::test]
#[serial]
async fn a_write_invalidates_the_written_sites_cached_readings() {
    if !kc::require_keycloak_or_skip("a_write_invalidates_cached_readings").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak_and_cache(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let written = provision_slot(&app, &jwt, "written").await;
    let untouched = provision_slot(&app, &jwt, "untouched").await;

    batch_one(&app, &jwt, &written, "2025-06-04T00:00:00Z", 101.0).await;
    batch_one(&app, &jwt, &untouched, "2025-06-04T00:00:00Z", 501.0).await;

    let window = "start=2025-06-04T00:00:00Z&end=2025-06-04T06:00:00Z";
    let written_uri = format!("/api/sites/{}/readings?{window}", written.site_id);
    let untouched_uri = format!("/api/sites/{}/readings?{window}", untouched.site_id);

    let before = probe(&app, &written_uri, Some(&jwt)).await;
    assert_eq!(before.status, 200, "written site readings: {}", before.body);
    assert_eq!(
        before.cache, "MISS",
        "an empty cache cannot hit: {}",
        before.body
    );
    assert_eq!(
        e2e::values_for(&before.json, &written.parameter_id),
        vec![101.0],
        "the first reading is served: {}",
        before.body
    );

    let untouched_before = probe(&app, &untouched_uri, Some(&jwt)).await;
    assert_eq!(
        untouched_before.status, 200,
        "untouched site readings: {}",
        untouched_before.body
    );
    assert_eq!(
        e2e::values_for(&untouched_before.json, &untouched.parameter_id),
        vec![501.0],
        "the other site's entry is primed too: {}",
        untouched_before.body
    );

    batch_one(&app, &jwt, &written, "2025-06-04T01:00:00Z", 102.0).await;

    let untouched_after = probe(&app, &untouched_uri, Some(&jwt)).await;
    assert_eq!(
        untouched_after.cache, "HIT",
        "invalidation is per-site: a write to one site must not drop another site's entry, which a \
         blanket invalidate_all would: {}",
        untouched_after.body
    );
    assert_eq!(
        untouched_after.body, untouched_before.body,
        "and the untouched site's body is unchanged"
    );

    let after = probe(&app, &written_uri, Some(&jwt)).await;
    assert_eq!(
        after.status, 200,
        "written site readings after the write: {}",
        after.body
    );
    assert_eq!(
        e2e::values_for(&after.json, &written.parameter_id),
        vec![101.0, 102.0],
        "a write inside the cached window must be reflected on the next identical read \
         (X-Cache {}): {}",
        after.cache,
        after.body
    );
    assert_eq!(
        after.cache, "MISS",
        "the write dropped the site's cached entry, so the read recomputed: {}",
        after.body
    );
}

// the unbounded-query freshness probe compares only MAX(time), so a reading backfilled
// below the cached maximum leaves the stale entry in place.
#[tokio::test]
#[serial]
async fn an_unbounded_read_reflects_a_backfilled_reading() {
    if !kc::require_keycloak_or_skip("an_unbounded_read_reflects_a_backfilled_reading").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak_and_cache(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    // `/ingest` touches the response cache nowhere (it only calls invalidate_all for corrections,
    // `readings/ingest.rs:286-289`), so what this test observes is the probe alone.
    let track = tracks::onboard_sensor_flow_track(&app, &jwt).await;
    let stream_id = track.stream_ids[0].clone();
    let parameter_id = track.parameter_id("TrkFlowDO").to_string();

    let (status, paired) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({ "site_parameter_id": &track.site_parameter_ids[0] }),
        &jwt,
    )
    .await;
    assert!((200..300).contains(&status), "pair ({status}): {paired}");

    ingest_one(&app, &jwt, &stream_id, "2025-06-02T01:00:00Z", 201.0).await;
    ingest_one(&app, &jwt, &stream_id, "2025-06-02T01:10:00Z", 202.0).await;

    // No `end`: the query is unbounded, which is the only shape the freshness probe runs for.
    let uri = format!(
        "/api/sites/{}/readings?start=2025-06-02T00:00:00Z",
        track.site_id
    );

    // The cache is on. Anchored on a bounded window, whose caching is the module's contract either
    // way, rather than on the unbounded query this test is about.
    let bounded_uri = format!("{uri}&end=2025-06-02T02:00:00Z");
    let bounded_first = probe(&app, &bounded_uri, Some(&jwt)).await;
    assert_eq!(
        bounded_first.status, 200,
        "bounded readings: {}",
        bounded_first.body
    );
    assert_eq!(
        bounded_first.cache, "MISS",
        "an empty cache cannot hit: {}",
        bounded_first.body
    );
    let bounded_repeat = probe(&app, &bounded_uri, Some(&jwt)).await;
    assert_eq!(
        bounded_repeat.cache, "HIT",
        "the identical bounded request is served from cache: {}",
        bounded_repeat.body
    );

    let first = probe(&app, &uri, Some(&jwt)).await;
    assert_eq!(first.status, 200, "unbounded readings: {}", first.body);
    assert_eq!(
        first.cache, "MISS",
        "the unbounded window is its own key: {}",
        first.body
    );
    assert_eq!(
        e2e::values_for(&first.json, &parameter_id),
        vec![201.0, 202.0],
        "both ingested readings are served: {}",
        first.body
    );

    ingest_one(&app, &jwt, &stream_id, "2025-06-02T01:20:00Z", 203.0).await;

    let appended = probe(&app, &uri, Some(&jwt)).await;
    assert_eq!(
        appended.cache, "MISS",
        "data past the cached maximum is what the probe is built to catch: {}",
        appended.body
    );
    assert_eq!(
        e2e::values_for(&appended.json, &parameter_id),
        vec![201.0, 202.0, 203.0],
        "the appended reading is served: {}",
        appended.body
    );

    // Late arrival: inside the served window, below the cached MAX(time).
    ingest_one(&app, &jwt, &stream_id, "2025-06-02T00:30:00Z", 200.5).await;

    let backfilled = probe(&app, &uri, Some(&jwt)).await;
    assert_eq!(
        e2e::values_for(&backfilled.json, &parameter_id),
        vec![200.5, 201.0, 202.0, 203.0],
        "a reading backfilled inside the served window must be served too, not hidden behind an \
         entry whose MAX(time) did not move (X-Cache {}): {}",
        backfilled.cache,
        backfilled.body
    );
}

// public entries are keyed `pub_readings:{project_code}:{site_code}:...`, a namespace no
// invalidation prefix targets, so a write to the site leaves the public response stale.
#[tokio::test]
#[serial]
async fn a_write_invalidates_the_sites_public_cached_readings() {
    if !kc::require_keycloak_or_skip("a_write_invalidates_the_sites_public_cached_readings").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak_and_cache(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let slot = provision_slot(&app, &jwt, "public").await;
    e2e::set_site_parameter_public(&db, &slot.site_parameter_id).await;

    batch_one(&app, &jwt, &slot, "2025-06-05T00:00:00Z", 601.0).await;
    batch_one(&app, &jwt, &slot, "2025-06-05T01:00:00Z", 602.0).await;

    let public_uri = "/api/public/cache-public/sites/cache-site-public/readings\
         ?start=2025-06-05T00:00:00Z&end=2025-06-05T06:00:00Z";

    let before = probe(&app, public_uri, None).await;
    assert_eq!(before.status, 200, "public readings: {}", before.body);
    let params = before.json["parameters"]
        .as_array()
        .expect("public parameters array");
    assert_eq!(
        params.len(),
        1,
        "one parameter is exposed publicly: {}",
        before.body
    );
    assert_eq!(
        before.json["times"].as_array().map(Vec::len),
        Some(2),
        "both readings are served publicly: {}",
        before.body
    );

    batch_one(&app, &jwt, &slot, "2025-06-05T02:00:00Z", 603.0).await;

    let private = probe(
        &app,
        &format!(
            "/api/sites/{}/readings?start=2025-06-05T00:00:00Z&end=2025-06-05T06:00:00Z",
            slot.site_id
        ),
        Some(&jwt),
    )
    .await;
    assert_eq!(
        e2e::values_for(&private.json, &slot.parameter_id),
        vec![601.0, 602.0, 603.0],
        "the third reading really landed, so anything missing from the public response below is \
         the cache and not the write: {}",
        private.body
    );

    let after = probe(&app, public_uri, None).await;
    assert_eq!(
        after.status, 200,
        "public readings after the write: {}",
        after.body
    );
    assert_eq!(
        after.json["times"].as_array().map(Vec::len),
        Some(3),
        "a write to a public site must reach that site's public cache entries (X-Cache {}): {}",
        after.cache,
        after.body
    );
    let values: Vec<f64> = after.json["parameters"][0]["values"]
        .as_array()
        .unwrap_or_else(|| panic!("no values in {}", after.body))
        .iter()
        .map(|v| v.as_f64().unwrap_or(f64::NAN))
        .collect();
    assert_eq!(
        values,
        vec![601.0, 602.0, 603.0],
        "with the new value: {}",
        after.body
    );
}

// Scope axis: the readings cache key carries no caller identity, so a primed entry
// must not become a way for an ungranted user to read another project's site.
#[tokio::test]
#[serial]
async fn a_primed_cache_entry_is_not_served_outside_the_callers_project_scope() {
    if !kc::require_keycloak_or_skip("a_primed_cache_entry_is_not_served_outside_scope").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak_and_cache(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let granted = provision_slot(&app, &admin, "granted").await;
    let ungranted = provision_slot(&app, &admin, "ungranted").await;
    batch_one(&app, &admin, &granted, "2025-06-06T00:00:00Z", 701.0).await;
    batch_one(&app, &admin, &ungranted, "2025-06-06T00:00:00Z", 801.0).await;

    kc::grant_project(
        &db,
        &kc::keycloak_user_id("river1").await,
        &granted.project_id,
    )
    .await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;

    let window = "start=2025-06-06T00:00:00Z&end=2025-06-06T06:00:00Z";
    let ungranted_uri = format!("/api/sites/{}/readings?{window}", ungranted.site_id);

    let primed = probe(&app, &ungranted_uri, Some(&admin)).await;
    assert_eq!(
        primed.status, 200,
        "an administrator sees every project: {}",
        primed.body
    );
    assert_eq!(
        e2e::values_for(&primed.json, &ungranted.parameter_id),
        vec![801.0],
        "the entry now in the cache holds the ungranted project's data: {}",
        primed.body
    );

    let denied = probe(&app, &ungranted_uri, Some(&river)).await;
    assert!(
        matches!(denied.status, 403 | 404),
        "a user without a grant on that project is refused, not served the cached body \
         ({}, X-Cache {}): {}",
        denied.status,
        denied.cache,
        denied.body
    );
    assert!(
        !denied.body.contains(&ungranted.parameter_id),
        "and the refusal leaks no part of the cached payload: {}",
        denied.body
    );

    let allowed = probe(
        &app,
        &format!("/api/sites/{}/readings?{window}", granted.site_id),
        Some(&river),
    )
    .await;
    assert_eq!(
        allowed.status, 200,
        "the same user reads the project they hold: {}",
        allowed.body
    );
    assert_eq!(
        e2e::values_for(&allowed.json, &granted.parameter_id),
        vec![701.0],
        "with its own data: {}",
        allowed.body
    );

    let reread = probe(&app, &ungranted_uri, Some(&admin)).await;
    assert_eq!(
        reread.status, 200,
        "the administrator still reads the site: {}",
        reread.body
    );
    assert_eq!(
        e2e::values_for(&reread.json, &ungranted.parameter_id),
        vec![801.0],
        "a refused request neither poisons nor evicts the entry: {}",
        reread.body
    );
}

/// Scenario: an operator pairs a stream that already holds history into a slot whose chart is open.
///
/// Expected behaviour: the pairing changes what the slot serves, so it announces the write like
/// every other path that changes stored reading values, and the open chart's next request misses.
#[tokio::test]
#[serial]
async fn pairing_a_stream_with_history_invalidates_the_slots_cached_readings() {
    if !kc::require_keycloak_or_skip("pairing_invalidates_cached_readings").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak_and_cache(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let slot = provision_slot(&app, &jwt, "paired").await;

    let stream_id = uuid::Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', 'vaisala', 'cache-pair-1', 'Cache Pair 1', true)"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, time, raw_value, replicate_index) \
             VALUES ('{stream_id}', '2025-06-04T02:00:00Z', 303.0, 0)"
        ),
    )
    .await;

    let window = "start=2025-06-04T00:00:00Z&end=2025-06-04T06:00:00Z";
    let uri = format!("/api/sites/{}/readings?{window}", slot.site_id);
    let before = probe(&app, &uri, Some(&jwt)).await;
    assert_eq!(before.status, 200, "slot readings: {}", before.body);
    assert_eq!(before.cache, "MISS", "an empty cache cannot hit");
    assert!(
        e2e::values_for(&before.json, &slot.parameter_id).is_empty(),
        "the unpaired stream's history is not the slot's yet: {}",
        before.body
    );
    assert_eq!(
        probe(&app, &uri, Some(&jwt)).await.cache,
        "HIT",
        "the entry is primed"
    );

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({ "site_parameter_id": slot.site_parameter_id }),
        &jwt,
    )
    .await;
    assert!((200..300).contains(&status), "pair ({status}): {body}");

    let after = probe(&app, &uri, Some(&jwt)).await;
    assert_eq!(
        after.cache, "MISS",
        "pairing changed what the slot serves, so the entry must be gone: {}",
        after.body
    );
    assert_eq!(
        e2e::values_for(&after.json, &slot.parameter_id),
        vec![303.0],
        "and the backfilled history is served: {}",
        after.body
    );
}
