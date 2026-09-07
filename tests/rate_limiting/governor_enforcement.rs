use serial_test::serial;

/// Authenticated endpoints must never return 429 regardless of request volume.
/// Rate limiting only applies to the public API tier.
#[tokio::test]
#[serial]
async fn authenticated_requests_are_not_rate_limited() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;

    let app = crate::common::build_test_app_with_rate_limiting(db);

    let mut status_codes = Vec::new();
    for _ in 0..50 {
        let (status, _) = crate::common::get_with_token(&app, "/api/projects", &token).await;
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
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    crate::common::exec(
        &db,
        &format!(
            "UPDATE projects SET is_public = true, public_code = 'test-river' WHERE id = '{}'",
            crate::common::PROJECT_ID
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE sites SET public_code = 'upstream' WHERE id = '{}'",
            crate::common::SITE1_ID,
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE site_parameters SET is_public = true WHERE id = '{}'",
            crate::common::PARAM_S1_TEMP_ID,
        ),
    )
    .await;

    let app = crate::common::build_test_app_with_rate_limiting(db);

    let url = "/api/public/test-river/sites/upstream/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T12:00:00Z";

    // Confirm the endpoint returns 200 before exhausting the burst
    let (status, _) = crate::common::get(&app, url).await;
    assert_eq!(status, 200, "Public endpoint should return data");

    let mut status_codes = Vec::new();
    for _ in 0..15 {
        let (status, _) = crate::common::get(&app, url).await;
        status_codes.push(status);
    }

    let rate_limited = status_codes.iter().filter(|&&s| s == 429).count();
    assert!(
        rate_limited > 0,
        "Public API should be rate-limited after burst exhaustion, but all 15 requests succeeded: {status_codes:?}"
    );
}

/// The peer address only reaches the limiter's key extractor when the router is served with
/// connect info wired in. Without it every caller keys to loopback and shares one bucket, so
/// this exercises the served socket rather than the router alone.
#[tokio::test]
#[serial]
async fn distinct_peers_get_distinct_buckets() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    crate::common::exec(
        &db,
        &format!(
            "UPDATE projects SET is_public = true, public_code = 'test-river' WHERE id = '{}'",
            crate::common::PROJECT_ID
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE sites SET public_code = 'upstream' WHERE id = '{}'",
            crate::common::SITE1_ID,
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE site_parameters SET is_public = true WHERE id = '{}'",
            crate::common::PARAM_S1_TEMP_ID,
        ),
    )
    .await;

    let app = crate::common::build_test_app_with_rate_limiting(db);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, river_db::routes::connected_service(app))
            .await
            .unwrap();
    });

    let url = format!(
        "http://127.0.0.1:{port}/api/public/test-river/sites/upstream/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T12:00:00Z"
    );
    let from = |ip: std::net::Ipv4Addr| {
        reqwest::Client::builder()
            .local_address(std::net::IpAddr::V4(ip))
            .build()
            .unwrap()
    };

    let first = from(std::net::Ipv4Addr::new(127, 0, 0, 1));
    let mut exhausted = false;
    for _ in 0..25 {
        if first.get(&url).send().await.unwrap().status() == 429 {
            exhausted = true;
            break;
        }
    }
    assert!(
        exhausted,
        "the first peer should exhaust its own burst within 25 requests"
    );

    let second = from(std::net::Ipv4Addr::new(127, 0, 0, 2));
    let status = second.get(&url).send().await.unwrap().status();
    assert_eq!(
        status, 200,
        "a request from another address must not spend the first peer's bucket"
    );
}
