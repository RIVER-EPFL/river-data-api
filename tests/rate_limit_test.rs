mod common;

use serial_test::serial;

/// Authenticated endpoints must never return 429 regardless of request volume.
/// Rate limiting only applies to the public API tier.
#[tokio::test]
#[serial]
async fn authenticated_requests_are_not_rate_limited() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;

    let app = common::build_test_app_with_rate_limiting(db);

    let mut status_codes = Vec::new();
    for _ in 0..50 {
        let (status, _) = common::get_with_token(&app, "/api/projects", &token).await;
        status_codes.push(status);
    }

    let rate_limited = status_codes.iter().filter(|&&s| s == 429).count();
    assert_eq!(
        rate_limited, 0,
        "Authenticated requests must never be rate-limited, got {rate_limited}/50 with 429"
    );

    let ok = status_codes.iter().filter(|&&s| s == 200).count();
    assert!(ok > 0, "At least some requests should succeed");
}

/// Public API tier must still enforce rate limiting.
/// With burst=10 and 2s refill, 15 rapid requests should trigger at least one 429.
#[tokio::test]
#[serial]
async fn public_api_is_still_rate_limited() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;

    common::exec(
        &db,
        &format!(
            "UPDATE projects SET is_public = true, public_slug = 'test-river' WHERE id = '{}'",
            common::PROJECT_ID
        ),
    )
    .await;
    common::exec(
        &db,
        &format!(
            "UPDATE sites SET public_slug = 'upstream' WHERE id = '{}'",
            common::SITE1_ID,
        ),
    )
    .await;
    common::exec(
        &db,
        &format!(
            "UPDATE site_parameters SET is_public = true WHERE id = '{}'",
            common::PARAM_S1_TEMP_ID,
        ),
    )
    .await;

    let app = common::build_test_app_with_rate_limiting(db);

    let url = "/api/public/test-river/sites/upstream/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T12:00:00Z";

    // Confirm the endpoint returns 200 before exhausting the burst
    let (status, _) = common::get(&app, url).await;
    assert_eq!(status, 200, "Public endpoint should return data");

    let mut status_codes = Vec::new();
    for _ in 0..15 {
        let (status, _) = common::get(&app, url).await;
        status_codes.push(status);
    }

    let rate_limited = status_codes.iter().filter(|&&s| s == 429).count();
    assert!(
        rate_limited > 0,
        "Public API should be rate-limited after burst exhaustion, but all 15 requests succeeded: {status_codes:?}"
    );
}
