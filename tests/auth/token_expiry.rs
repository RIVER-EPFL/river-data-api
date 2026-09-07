//! Token expiry boundaries at the route. The expiry decision itself is a pure function, tested
//! inline in `api_tokens::service`; what these prove is that the routes consult it.

use chrono::{Duration, Utc};
use serial_test::serial;

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app)
}

#[tokio::test]
#[serial]
async fn already_expired_token_is_rejected() {
    let (db, app) = setup().await;
    let token = crate::common::seed_api_token_with_expiry(
        &db,
        crate::common::full_permissions(),
        None,
        Utc::now() - Duration::seconds(1),
    )
    .await;

    let (s, _) = crate::common::get_with_token(&app, "/api/sites", &token).await;
    assert_eq!(
        s, 401,
        "a token whose expiry is in the past must be rejected on first use"
    );
}

#[tokio::test]
#[serial]
async fn far_future_expiry_is_accepted() {
    let (db, app) = setup().await;
    let token = crate::common::seed_api_token_with_expiry(
        &db,
        crate::common::full_permissions(),
        None,
        Utc::now() + Duration::days(365),
    )
    .await;

    let (s, _) = crate::common::get_with_token(&app, "/api/sites", &token).await;
    assert_eq!(s, 200, "a token with a far-future expiry must authenticate");
}
