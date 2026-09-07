//! The byte ceiling. `CACHE_MAX_BYTES` is 200MB in production and 10MB in the cache fixture, so
//! nothing else in the estate reaches the eviction path at all: these tests run against a ceiling
//! small enough that a second response displaces the first.
//!
//! Run: cargo test --test cache eviction -- --test-threads=1

use chrono::Utc;
use river_db::common::AppState;
use river_db::routes::cache;
use serial_test::serial;
use std::time::Duration;

use crate::common::{
    PROJECT_ID, SITE1_ID, build_test_app_with_cache_ceiling, cleanup_test_db, exec, setup_test_db,
};

const PROJECT_CODE: &str = "evict-proj";
const SITE1_CODE: &str = "evict-site";

/// The ceiling, and a body that on its own fits under it. Two bodies do not.
const CEILING_BYTES: u64 = 4096;
const BODY_BYTES: usize = 3000;

async fn seed_public_site(db: &sea_orm::DatabaseConnection) {
    cleanup_test_db(db).await;
    exec(
        db,
        &format!(
            "INSERT INTO projects (id, name, description, data_source, is_public, public_code) \
             VALUES ('{PROJECT_ID}', 'Eviction project', 'cache tests', 'test', true, '{PROJECT_CODE}')"
        ),
    )
    .await;
    exec(
        db,
        &format!(
            "INSERT INTO sites (id, project_id, name, latitude, longitude, altitude_m, public_code) \
             VALUES ('{SITE1_ID}', '{PROJECT_ID}', 'Eviction site', 46.1, 7.1, 500.0, '{SITE1_CODE}')"
        ),
    )
    .await;
}

fn key(suffix: &str) -> String {
    format!("readings:{SITE1_ID}:{suffix}")
}

fn body(fill: char) -> Vec<u8> {
    std::iter::repeat_n(fill as u8, BODY_BYTES).collect()
}

async fn cached(state: &AppState, key: &str) -> Option<Vec<u8>> {
    cache::get_cached(state, key, &[], Some(Utc::now()))
        .await
        .map(|bytes| bytes.to_vec())
}

/// Moka evicts on a background task, so a size-driven test waits for it rather than assuming it
/// has already run. Returns false if both entries are still served after the wait.
async fn wait_until_one_is_dropped(state: &AppState, keys: &[String]) -> bool {
    for _ in 0..200 {
        let mut present = 0;
        for k in keys {
            if cached(state, k).await.is_some() {
                present += 1;
            }
        }
        if present < keys.len() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test]
#[serial]
async fn a_response_over_the_ceiling_displaces_an_earlier_one() {
    let db = setup_test_db().await;
    seed_public_site(&db).await;
    let (_app, state) = build_test_app_with_cache_ceiling(db.clone(), CEILING_BYTES);

    let first = key("first");
    let second = key("second");
    cache::store_cached(&state, first.clone(), body('a'), None).await;
    assert_eq!(
        cached(&state, &first).await.map(|b| b.len()),
        Some(BODY_BYTES),
        "one body fits under the ceiling"
    );

    cache::store_cached(&state, second.clone(), body('b'), None).await;
    assert!(
        wait_until_one_is_dropped(&state, &[first.clone(), second.clone()]).await,
        "two bodies do not, so one of them is evicted"
    );

    cleanup_test_db(&db).await;
}

/// Eviction is a miss, not a wrong answer. Which key moka's admission policy drops is its own
/// business, so what is asserted is the pair that matters: the ceiling holds, and every entry
/// still served carries the bytes it was stored with rather than a stale or truncated body.
#[tokio::test]
#[serial]
async fn what_survives_the_ceiling_is_served_whole() {
    let db = setup_test_db().await;
    seed_public_site(&db).await;
    let (_app, state) = build_test_app_with_cache_ceiling(db.clone(), CEILING_BYTES);

    let fills = ['a', 'b', 'c', 'd', 'e', 'f'];
    let keys: Vec<String> = fills.iter().map(|f| key(&f.to_string())).collect();
    for (k, fill) in keys.iter().zip(fills) {
        cache::store_cached(&state, k.clone(), body(fill), None).await;
    }
    assert!(
        wait_until_one_is_dropped(&state, &keys).await,
        "6 bodies of {BODY_BYTES} bytes do not all fit under {CEILING_BYTES}"
    );

    let mut served = 0;
    for (k, fill) in keys.iter().zip(fills) {
        let Some(bytes) = cached(&state, k).await else {
            continue;
        };
        served += 1;
        assert_eq!(bytes.len(), BODY_BYTES, "{k} is served truncated");
        assert!(
            bytes.iter().all(|b| *b == fill as u8),
            "{k} is served another entry's body"
        );
    }
    assert!(served > 0, "the cache evicted everything rather than the excess");

    cleanup_test_db(&db).await;
}

/// A ceiling of zero is the documented off switch: `caching_enabled` reads both limits, so nothing
/// is stored at all rather than everything being stored and immediately evicted.
#[tokio::test]
#[serial]
async fn a_zero_ceiling_stores_nothing() {
    let db = setup_test_db().await;
    seed_public_site(&db).await;
    let (_app, state) = build_test_app_with_cache_ceiling(db.clone(), 0);

    let key = key("zero");
    cache::store_cached(&state, key.clone(), body('a'), None).await;
    assert_eq!(cached(&state, &key).await, None);

    cleanup_test_db(&db).await;
}
