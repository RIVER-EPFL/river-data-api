//! Where each tier's bucket actually runs out and refills. `governor_enforcement.rs` shows that
//! the public tier limits and the authenticated one does not at the shipped settings; these drive
//! the knobs to values whose exact boundary is assertable.
//!
//! Run: cargo test --test rate_limiting boundaries -- --test-threads=1

use serial_test::serial;

const PUBLIC_URL: &str = "/api/public/test-river/sites/upstream/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T12:00:00Z";

/// A public project with one public site and one public parameter: the readings body is not the
/// subject, only how many times it is served.
async fn seed_public(db: &sea_orm::DatabaseConnection) {
    crate::common::cleanup_test_db(db).await;
    crate::common::seed_test_data(db).await;
    crate::common::exec(
        db,
        &format!(
            "UPDATE projects SET is_public = true, public_code = 'test-river' WHERE id = '{}'",
            crate::common::PROJECT_ID
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "UPDATE sites SET public_code = 'upstream' WHERE id = '{}'",
            crate::common::SITE1_ID
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "UPDATE site_parameters SET is_public = true WHERE id = '{}'",
            crate::common::PARAM_S1_TEMP_ID
        ),
    )
    .await;
}

/// The burst is the number of requests that pass, and the one after it is refused: a bucket of
/// `burst` cells, refilled one per period.
#[tokio::test]
#[serial]
async fn the_public_burst_is_spent_exactly_and_the_next_request_is_refused() {
    let db = crate::common::setup_test_db().await;
    seed_public(&db).await;
    let burst = 3;
    let app = crate::common::build_test_app_with_limits(db, burst, 60, 100_000);

    for i in 0..burst {
        let (status, body) = crate::common::get(&app, PUBLIC_URL).await;
        assert_eq!(status, 200, "request {i} is inside the burst: {body}");
    }
    let (status, _) = crate::common::get(&app, PUBLIC_URL).await;
    assert_eq!(status, 429, "the request after the burst is refused");
}

/// One cell per period, so the refused caller is served again once a period has passed and is
/// refused again immediately after: the bucket refills at the rate, not to full.
#[tokio::test]
#[serial]
async fn a_refused_caller_is_served_again_one_period_later() {
    let db = crate::common::setup_test_db().await;
    seed_public(&db).await;
    let app = crate::common::build_test_app_with_limits(db, 1, 1, 100_000);

    assert_eq!(crate::common::get(&app, PUBLIC_URL).await.0, 200);
    assert_eq!(
        crate::common::get(&app, PUBLIC_URL).await.0,
        429,
        "the single cell is spent"
    );

    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert_eq!(
        crate::common::get(&app, PUBLIC_URL).await.0,
        200,
        "one period later one cell is back"
    );
    assert_eq!(
        crate::common::get(&app, PUBLIC_URL).await.0,
        429,
        "and only one"
    );
}

/// The authenticated tier is limited too; it only looks unlimited because the shipped burst is far
/// above what any caller sends. Its own ceiling is where it is set.
#[tokio::test]
#[serial]
async fn the_authenticated_tier_has_a_ceiling_of_its_own() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let burst = 4;
    let app = crate::common::build_test_app_with_limits(db, 100_000, 60, burst);

    for i in 0..burst {
        let (status, _) = crate::common::get_with_token(&app, "/api/projects", &token).await;
        assert_eq!(status, 200, "request {i} is inside the authenticated burst");
    }
    let (status, _) = crate::common::get_with_token(&app, "/api/projects", &token).await;
    assert_eq!(
        status, 429,
        "the authenticated tier refuses past its own burst, before the token is even read"
    );
}

/// `DISABLE_RATE_LIMITING` removes both layers rather than raising them, so a volume far past
/// either burst is served in full. This is the setting every other theme runs under.
#[tokio::test]
#[serial]
async fn the_disable_switch_removes_both_tiers() {
    let db = crate::common::setup_test_db().await;
    seed_public(&db).await;
    // test_config() disables rate limiting; the public tier would otherwise refuse after 10.
    let app = crate::common::build_test_app(db);

    for i in 0..20 {
        let (status, _) = crate::common::get(&app, PUBLIC_URL).await;
        assert_eq!(status, 200, "request {i} is served with the limiter off");
    }
}
