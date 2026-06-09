//! Per-token rate limiting boundaries, and the deliberate exemption of sync session tokens
//! (which do bulk backfills and must never be throttled). Per-token 429 + unlimited behaviour is
//! also covered in `e2e_api_token_lifecycle_test`; here we add refill-recovery and the sync path.


use serial_test::serial;

use crate::common::fixtures::{GLOBAL_PARAM_TEMP_ID, PROJECT_ID, SITE1_ID};

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app)
}

fn reading_body() -> serde_json::Value {
    let t = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();
    serde_json::json!({
        "readings": [{ "site_id": SITE1_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": t, "raw_value": 1.0 }]
    })
}

#[tokio::test]
#[serial]
async fn per_token_limit_recovers_after_refill() {
    let (db, app) = setup().await;
    let key = crate::common::seed_api_token_with_rate_limit(
        &db,
        crate::common::perms(true, true, false, true),
        Some(PROJECT_ID),
        2,
    )
    .await;

    // Burst until the 2/s ceiling rejects with 429.
    let mut hit_429 = false;
    for _ in 0..10 {
        let (s, _) = crate::common::post_json_with_token(&app, "/api/readings/batch", &reading_body(), &key).await;
        if s == 429 {
            hit_429 = true;
            break;
        }
    }
    assert!(hit_429, "a 2/s key must 429 under a burst");

    // After the window refills, the key works again.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let (s, _) = crate::common::post_json_with_token(&app, "/api/readings/batch", &reading_body(), &key).await;
    assert_ne!(s, 429, "the key must recover once the rate-limit window refills, got {s}");
}

#[tokio::test]
#[serial]
async fn sync_session_token_is_never_throttled() {
    let (db, app) = setup().await;
    // Sync microservices do bulk backfills; their session tokens carry no per-token limit.
    let (sync_token, _service_id) = crate::common::seed_sync_session_token(&db).await;

    let mut statuses = Vec::new();
    for _ in 0..25 {
        let (s, _) = crate::common::post_json_with_token(&app, "/api/readings/batch", &reading_body(), &sync_token).await;
        statuses.push(s);
    }
    assert!(
        statuses.iter().all(|&s| s != 429),
        "a sync session token must never be rate-limited (bulk feeds), got {statuses:?}"
    );
    assert!(
        statuses.iter().any(|&s| s == 200),
        "the sync token should actually be ingesting (some 200s), got {statuses:?}"
    );
}
