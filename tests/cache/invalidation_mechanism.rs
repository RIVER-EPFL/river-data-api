//! The invalidation mechanism itself, driven through the cache API rather than through HTTP.
//!
//! `key_and_invalidation.rs` proves what a dashboard user is served; this file pins the mechanism
//! that makes it so, including the cases an HTTP sequence cannot reach cheaply: a key naming no
//! site, an entry stored after an invalidation, and the event-bus subscriber that invalidates for
//! writers which never call this module.
//!
//! Every test uses the cache-enabled builder, a bounded `end` (so the freshness probe never runs
//! and a hit is deterministic), and no `param_ids` (nothing here is about the probe).
//!
//! Run: cargo test --test cache -- --test-threads=1

use chrono::Utc;
use river_db::common::{AppEvent, AppState};
use river_db::routes::cache;
use serial_test::serial;
use std::time::Duration;
use uuid::Uuid;

use crate::common::{
    PROJECT_ID, SITE1_ID, SITE2_ID, build_test_app_with_cache_and_state, cleanup_test_db, exec,
    setup_test_db,
};

const PROJECT_CODE: &str = "cache-proj";
const SITE1_CODE: &str = "site-one";
const SITE2_CODE: &str = "site-two";

fn site1() -> Uuid {
    Uuid::parse_str(SITE1_ID).unwrap()
}

fn site2() -> Uuid {
    Uuid::parse_str(SITE2_ID).unwrap()
}

/// A public project holding two public sites. No parameters and no readings: what is under test is
/// which cached bytes survive a write, not what the endpoints compute.
async fn seed_public_sites(db: &sea_orm::DatabaseConnection) {
    cleanup_test_db(db).await;
    exec(
        db,
        &format!(
            "INSERT INTO projects (id, name, description, data_source, is_public, public_code) \
             VALUES ('{PROJECT_ID}', 'Cache project', 'cache tests', 'test', true, '{PROJECT_CODE}')"
        ),
    )
    .await;
    exec(
        db,
        &format!(
            "INSERT INTO sites (id, project_id, name, latitude, longitude, altitude_m, public_code) \
             VALUES ('{SITE1_ID}', '{PROJECT_ID}', 'Site One', 46.1, 7.1, 500.0, '{SITE1_CODE}'), \
                    ('{SITE2_ID}', '{PROJECT_ID}', 'Site Two', 46.2, 7.2, 600.0, '{SITE2_CODE}')"
        ),
    )
    .await;
}

fn private_key(site: &str) -> String {
    cache::cache_key(
        "readings",
        &[site, "2025-06-01T00:00:00Z", "2025-06-01T06:00:00Z", "json"],
    )
}

fn public_key(site_code: &str) -> String {
    cache::cache_key(
        "pub_readings",
        &[
            PROJECT_CODE,
            site_code,
            "Depth",
            "2025-06-01T00:00:00Z",
            "2025-06-01T06:00:00Z",
        ],
    )
}

async fn store(state: &AppState, key: &str, body: &str) {
    cache::store_cached(state, key.to_string(), body.as_bytes().to_vec(), None).await;
}

async fn cached(state: &AppState, key: &str) -> Option<String> {
    cache::get_cached(state, key, &[], Some(Utc::now()))
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
}

/// The bus subscriber runs on its own task, so a bus-driven test waits for it rather than assuming
/// it has already run. Returns false if the entry is still served after the wait.
async fn wait_until_dropped(state: &AppState, key: &str) -> bool {
    for _ in 0..200 {
        if cached(state, key).await.is_none() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test]
#[serial]
async fn a_stored_entry_is_served_back_until_its_site_is_invalidated() {
    let db = setup_test_db().await;
    seed_public_sites(&db).await;
    let (_app, state) = build_test_app_with_cache_and_state(db.clone());

    let key = private_key(SITE1_ID);
    store(&state, &key, "first").await;
    assert_eq!(
        cached(&state, &key).await.as_deref(),
        Some("first"),
        "a bounded entry is served back, which is what the rest of this file removes"
    );

    cache::invalidate_site(&state.response_cache, site1());
    assert_eq!(
        cached(&state, &key).await,
        None,
        "the written site's entry is dropped"
    );

    store(&state, &key, "second").await;
    assert_eq!(
        cached(&state, &key).await.as_deref(),
        Some("second"),
        "an entry stored after the invalidation is not caught by it"
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn invalidating_a_site_leaves_every_other_site_alone() {
    let db = setup_test_db().await;
    seed_public_sites(&db).await;
    let (_app, state) = build_test_app_with_cache_and_state(db.clone());

    let written = private_key(SITE1_ID);
    let untouched = private_key(SITE2_ID);
    store(&state, &written, "written").await;
    store(&state, &untouched, "untouched").await;

    cache::invalidate_site(&state.response_cache, site1());

    assert_eq!(
        cached(&state, &written).await,
        None,
        "the written site goes"
    );
    assert_eq!(
        cached(&state, &untouched).await.as_deref(),
        Some("untouched"),
        "the other site stays, which a blanket invalidate_all would not"
    );

    cleanup_test_db(&db).await;
}

// the public namespace carries codes rather than a site UUID, and one call must still
// reach it.
#[tokio::test]
#[serial]
async fn invalidating_a_site_reaches_its_public_entries_too() {
    let db = setup_test_db().await;
    seed_public_sites(&db).await;
    let (_app, state) = build_test_app_with_cache_and_state(db.clone());

    let public_written = public_key(SITE1_CODE);
    let public_other = public_key(SITE2_CODE);
    let private_written = private_key(SITE1_ID);
    store(&state, &public_written, "public written").await;
    store(&state, &public_other, "public other").await;
    store(&state, &private_written, "private written").await;

    assert_eq!(
        cache::site_of_key(&state, &public_written).await,
        Some(site1()),
        "a public key resolves to the site its codes name"
    );

    cache::invalidate_site(&state.response_cache, site1());

    assert_eq!(
        cached(&state, &public_written).await,
        None,
        "the public entry for the written site is dropped by the same call as the private one"
    );
    assert_eq!(cached(&state, &private_written).await, None);
    assert_eq!(
        cached(&state, &public_other).await.as_deref(),
        Some("public other"),
        "and the other site's public entry survives"
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn an_entry_naming_no_site_is_dropped_by_any_invalidation() {
    let db = setup_test_db().await;
    seed_public_sites(&db).await;
    let (_app, state) = build_test_app_with_cache_and_state(db.clone());

    let unattributed = cache::cache_key("readings", &["not-a-uuid", "json"]);
    let unknown_project = public_key(SITE1_CODE).replace(PROJECT_CODE, "no-such-project");
    store(&state, &unattributed, "unattributed").await;
    store(&state, &unknown_project, "unknown project").await;

    assert_eq!(cache::site_of_key(&state, &unattributed).await, None);
    assert_eq!(cache::site_of_key(&state, &unknown_project).await, None);

    cache::invalidate_site(&state.response_cache, site2());

    assert_eq!(
        cached(&state, &unattributed).await,
        None,
        "an entry that cannot be shown to be unaffected is dropped"
    );
    assert_eq!(cached(&state, &unknown_project).await, None);

    cleanup_test_db(&db).await;
}

// The writers still spell invalidation as a namespace prefix; the shim must reach the site's
// entries in every namespace, not just the one named.
#[tokio::test]
#[serial]
async fn a_namespace_prefix_invalidates_the_whole_site() {
    let db = setup_test_db().await;
    seed_public_sites(&db).await;
    let (_app, state) = build_test_app_with_cache_and_state(db.clone());

    let readings = private_key(SITE1_ID);
    let aggregates = cache::cache_key("aggregates", &[SITE1_ID, "hourly"]);
    let public = public_key(SITE1_CODE);
    let other_site = private_key(SITE2_ID);
    store(&state, &readings, "readings").await;
    store(&state, &aggregates, "aggregates").await;
    store(&state, &public, "public").await;
    store(&state, &other_site, "other site").await;

    cache::invalidate_prefix(&state, &format!("readings:{SITE1_ID}")).await;

    assert_eq!(cached(&state, &readings).await, None);
    assert_eq!(
        cached(&state, &aggregates).await,
        None,
        "the namespace in the prefix does not narrow what is dropped"
    );
    assert_eq!(cached(&state, &public).await, None);
    assert_eq!(
        cached(&state, &other_site).await.as_deref(),
        Some("other site"),
        "the prefix still names one site"
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn a_prefix_naming_no_site_drops_everything() {
    let db = setup_test_db().await;
    seed_public_sites(&db).await;
    let (_app, state) = build_test_app_with_cache_and_state(db.clone());

    let one = private_key(SITE1_ID);
    let two = private_key(SITE2_ID);
    store(&state, &one, "one").await;
    store(&state, &two, "two").await;

    cache::invalidate_prefix(&state, "readings").await;

    assert_eq!(cached(&state, &one).await, None);
    assert_eq!(
        cached(&state, &two).await,
        None,
        "a prefix that resolves to no site cannot be applied precisely, so it applies bluntly"
    );

    cleanup_test_db(&db).await;
}

// Scenario: a writer announces a write on the event bus and calls nothing in the cache module.
// Expected behaviour: the written site's entries go anyway.
#[tokio::test]
#[serial]
async fn the_write_bus_invalidates_the_site_it_names() {
    let db = setup_test_db().await;
    seed_public_sites(&db).await;
    let (_app, state) = build_test_app_with_cache_and_state(db.clone());

    let written = private_key(SITE1_ID);
    let written_public = public_key(SITE1_CODE);
    let untouched = private_key(SITE2_ID);
    store(&state, &written, "written").await;
    store(&state, &written_public, "written public").await;
    store(&state, &untouched, "untouched").await;

    let _ = state.events.send(AppEvent::DataIngested {
        site_id: Some(site1()),
        parameter_id: None,
        stream_id: Uuid::new_v4(),
        count: 1,
    });

    assert!(
        wait_until_dropped(&state, &written).await,
        "an ingest event drops the site's private entry"
    );
    assert!(
        wait_until_dropped(&state, &written_public).await,
        "and its public entry"
    );
    assert_eq!(
        cached(&state, &untouched).await.as_deref(),
        Some("untouched"),
        "while another site's entry is untouched"
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn an_ingest_event_without_a_site_invalidates_nothing() {
    let db = setup_test_db().await;
    seed_public_sites(&db).await;
    let (_app, state) = build_test_app_with_cache_and_state(db.clone());

    let key = private_key(SITE1_ID);
    store(&state, &key, "kept").await;

    let _ = state.events.send(AppEvent::DataIngested {
        site_id: None,
        parameter_id: None,
        stream_id: Uuid::new_v4(),
        count: 1,
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        cached(&state, &key).await.as_deref(),
        Some("kept"),
        "an unpaired stream's readings reach no site endpoint, so no site entry is stale"
    );

    cleanup_test_db(&db).await;
}

// A job's own completion says nothing about the served bytes: `readings_updated` carries whatever
// the job returned, alarm events included. A job that rewrote readings announces that the same way
// every other writer does, and only that announcement invalidates.
#[tokio::test]
#[serial]
async fn a_job_completion_alone_invalidates_nothing() {
    let db = setup_test_db().await;
    seed_public_sites(&db).await;
    let (_app, state) = build_test_app_with_cache_and_state(db.clone());

    let key = private_key(SITE1_ID);
    store(&state, &key, "kept").await;

    let job_id = Uuid::new_v4();
    let _ = state.events.send(AppEvent::JobCompleted {
        job_id,
        status: "completed".to_string(),
        readings_updated: Some(7),
        error_message: None,
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        cached(&state, &key).await.as_deref(),
        Some("kept"),
        "an alarm rebuild reporting seven events must not drop a site's readings"
    );

    let _ = state.events.send(AppEvent::DataIngested {
        site_id: Some(site1()),
        parameter_id: None,
        stream_id: Uuid::new_v4(),
        count: 7,
    });
    assert!(
        wait_until_dropped(&state, &key).await,
        "the announcement of the write is what invalidates"
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn invalidating_a_site_with_nothing_cached_is_harmless() {
    let db = setup_test_db().await;
    seed_public_sites(&db).await;
    let (_app, state) = build_test_app_with_cache_and_state(db.clone());

    cache::invalidate_site(&state.response_cache, site1());
    cache::invalidate_site(&state.response_cache, site1());

    let key = private_key(SITE1_ID);
    store(&state, &key, "after").await;
    assert_eq!(
        cached(&state, &key).await.as_deref(),
        Some("after"),
        "invalidations registered against an empty cache do not outlive themselves"
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn the_cacheless_builder_stores_nothing() {
    let db = setup_test_db().await;
    seed_public_sites(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let key = private_key(SITE1_ID);
    store(&state, &key, "ignored").await;
    assert_eq!(
        cached(&state, &key).await,
        None,
        "with the cache off there is nothing to invalidate and nothing to serve"
    );

    cleanup_test_db(&db).await;
}
