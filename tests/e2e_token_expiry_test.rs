//! Token expiry boundaries. A ~5s-ahead expiry is used so the test exercises real expiration
//! quickly; the validation cache (1s TTL in tests) must not mask an expired token.

mod common;

use chrono::{Duration, Utc};
use serial_test::serial;

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router) {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;
    let app = common::build_test_app(db.clone());
    (db, app)
}

#[tokio::test]
#[serial]
async fn token_expiring_in_five_seconds_works_then_stops() {
    let (db, app) = setup().await;
    let token = common::seed_api_token_with_expiry(
        &db,
        common::full_permissions(),
        None,
        Utc::now() + Duration::seconds(5),
    )
    .await;

    // Works now — and this use caches it as valid, so the post-expiry check also proves the cache
    // cannot keep an expired token alive (expiry is re-checked on every cache hit).
    let (s, _) = common::get_with_token(&app, "/api/sites", &token).await;
    assert_eq!(s, 200, "token must work before its expiry");

    tokio::time::sleep(std::time::Duration::from_secs(6)).await;

    let (s, _) = common::get_with_token(&app, "/api/sites", &token).await;
    assert_eq!(s, 401, "token must be rejected once past its expiry, even if recently cached");
}

#[tokio::test]
#[serial]
async fn already_expired_token_is_rejected() {
    let (db, app) = setup().await;
    let token = common::seed_api_token_with_expiry(
        &db,
        common::full_permissions(),
        None,
        Utc::now() - Duration::seconds(1),
    )
    .await;

    let (s, _) = common::get_with_token(&app, "/api/sites", &token).await;
    assert_eq!(s, 401, "a token whose expiry is in the past must be rejected on first use");
}

#[tokio::test]
#[serial]
async fn far_future_expiry_is_accepted() {
    let (db, app) = setup().await;
    let token = common::seed_api_token_with_expiry(
        &db,
        common::full_permissions(),
        None,
        Utc::now() + Duration::days(365),
    )
    .await;

    let (s, _) = common::get_with_token(&app, "/api/sites", &token).await;
    assert_eq!(s, 200, "a token with a far-future expiry must authenticate");
}
