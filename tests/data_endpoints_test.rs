//! End-to-end tests for data endpoints: readings, aggregates, alarms.
//!
//! Run with: cargo test --test data_endpoints_test
//! Requires: DATABASE_URL pointing to a TimescaleDB instance.

mod common;

use serial_test::serial;

// ============================================================================
// Helper: setup, cleanup, seed, and build app
// ============================================================================

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router) {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;
    let app = common::build_test_app(db.clone());
    (db, app)
}

// ============================================================================
// Readings endpoint: GET /api/private/sites/{site_id}/readings
// ============================================================================

#[tokio::test]
#[serial]
async fn test_readings_basic_time_range() {
    let (_db, app) = setup().await;
    let site_id = common::SITE1_ID;

    let (status, body) = common::get_json(
        &app,
        &format!(
            "/api/private/sites/{site_id}/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T12:00:00Z"
        ),
    )
    .await;

    assert_eq!(status, 200);

    // Top-level structure
    assert!(body["project"].is_object(), "should have project object");
    assert!(body["site"].is_object(), "should have site object");
    assert!(body["start"].is_string(), "should have start timestamp");
    assert!(body["end"].is_string(), "should have end timestamp");
    assert!(body["times"].is_array(), "should have times array");
    assert!(
        body["parameters"].is_array(),
        "should have parameters array"
    );

    // Site reference
    assert_eq!(body["site"]["id"].as_str().unwrap(), site_id);

    // 12 hours at 10-min intervals = 73 readings (inclusive boundaries: 00:00..=12:00)
    let times = body["times"].as_array().unwrap();
    assert_eq!(
        times.len(),
        73,
        "12h at 10-min intervals (inclusive) = 73 readings"
    );

    // Site 1 has 5 parameters
    let params = body["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 5, "site 1 should have 5 parameters");

    // Each parameter has values array aligned with times
    for param in params {
        assert!(param["id"].is_string(), "param should have id");
        assert!(param["name"].is_string(), "param should have name");
        assert!(param["type"].is_string(), "param should have type");

        let values = param["values"].as_array().unwrap();
        assert_eq!(
            values.len(),
            times.len(),
            "values array should match times length"
        );

        // All values should be numbers (not null) for the seeded range
        for v in values {
            assert!(v.is_number(), "value should be a number, got: {v}");
        }
    }
}

#[tokio::test]
#[serial]
async fn test_readings_sensor_types_filter() {
    let (_db, app) = setup().await;
    let site_id = common::SITE1_ID;

    let (status, body) = common::get_json(
        &app,
        &format!(
            "/api/private/sites/{site_id}/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T12:00:00Z&sensor_types=DO_Temperature,Turbidity"
        ),
    )
    .await;

    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 2, "should return only 2 filtered parameters");

    let types: Vec<&str> = params.iter().map(|p| p["type"].as_str().unwrap()).collect();
    assert!(types.contains(&"DO_Temperature"));
    assert!(types.contains(&"Turbidity"));
}

#[tokio::test]
#[serial]
async fn test_readings_with_alarms() {
    let (_db, app) = setup().await;
    let site_id = common::SITE1_ID;

    // Use full 48h range to include injected alarm values at steps 50, 100, 200
    let (status, body) = common::get_json(
        &app,
        &format!(
            "/api/private/sites/{site_id}/readings?start=2025-01-15T00:00:00Z&end=2025-01-17T00:00:00Z&alarms=true"
        ),
    )
    .await;

    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    assert!(!params.is_empty());

    let mut found_nonzero_severity = false;
    for param in params {
        // When alarms=true, each parameter should have severities array
        assert!(
            param["severities"].is_array(),
            "parameter should have severities array when alarms=true"
        );

        let severities = param["severities"].as_array().unwrap();
        let values = param["values"].as_array().unwrap();
        assert_eq!(
            severities.len(),
            values.len(),
            "severities should match values length"
        );

        for sev in severities {
            if !sev.is_null() {
                let s = sev.as_i64().unwrap();
                assert!(
                    s == 0 || s == 1 || s == 2,
                    "severity should be 0, 1, or 2, got: {s}"
                );
                if s > 0 {
                    found_nonzero_severity = true;
                }
            }
        }
    }

    assert!(
        found_nonzero_severity,
        "should have at least one non-zero severity from seeded alarm-triggering values"
    );
}

#[tokio::test]
#[serial]
async fn test_readings_no_time_range() {
    let (_db, app) = setup().await;
    let site_id = common::SITE1_ID;

    // No start/end → defaults to full data range
    let (status, body) =
        common::get_json(&app, &format!("/api/private/sites/{site_id}/readings")).await;

    assert_eq!(status, 200);

    let times = body["times"].as_array().unwrap();
    assert!(
        !times.is_empty(),
        "should return data when no time range specified"
    );

    // start/end in response should reflect actual data range
    assert!(body["start"].is_string(), "response should have start");
    assert!(body["end"].is_string(), "response should have end");
}

#[tokio::test]
#[serial]
async fn test_readings_nonexistent_site_returns_404() {
    let (_db, app) = setup().await;

    let (status, _body) = common::get(
        &app,
        "/api/private/sites/00000000-0000-4000-a000-999999999999/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T12:00:00Z",
    )
    .await;

    assert_eq!(status, 404, "nonexistent site should return 404");
}

#[tokio::test]
#[serial]
async fn test_readings_invalid_time_range_returns_400() {
    let (_db, app) = setup().await;
    let site_id = common::SITE1_ID;

    // end before start
    let (status, _body) = common::get(
        &app,
        &format!(
            "/api/private/sites/{site_id}/readings?start=2025-01-16T00:00:00Z&end=2025-01-15T00:00:00Z"
        ),
    )
    .await;

    assert_eq!(status, 400, "end before start should return 400");
}

// ============================================================================
// Aggregates endpoint: GET /api/private/sites/{site_id}/aggregates/{resolution}
// ============================================================================

#[tokio::test]
#[serial]
async fn test_aggregates_hourly() {
    let (_db, app) = setup().await;
    let site_id = common::SITE1_ID;

    let (status, body) = common::get_json(
        &app,
        &format!(
            "/api/private/sites/{site_id}/aggregates/hourly?start=2025-01-15T00:00:00Z&end=2025-01-16T00:00:00Z"
        ),
    )
    .await;

    assert_eq!(status, 200);

    // Top-level structure
    assert!(body["project"].is_object(), "should have project object");
    assert!(body["site"].is_object(), "should have site object");
    assert_eq!(
        body["resolution"].as_str().unwrap(),
        "hourly",
        "resolution should be 'hourly'"
    );
    assert!(body["start"].is_string());
    assert!(body["end"].is_string());
    assert!(body["times"].is_array());
    assert!(body["parameters"].is_array());

    // time_bucket can produce 25 buckets for a 24h range (inclusive boundary)
    let times = body["times"].as_array().unwrap();
    assert!(
        times.len() == 24 || times.len() == 25,
        "expected 24 or 25 hourly buckets, got {}",
        times.len()
    );

    let params = body["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 5, "site 1 should have 5 parameters");

    for param in params {
        assert!(param["id"].is_string());
        assert!(param["name"].is_string());
        assert!(param["type"].is_string());

        let avg = param["avg"].as_array().unwrap();
        let min = param["min"].as_array().unwrap();
        let max = param["max"].as_array().unwrap();
        let count = param["count"].as_array().unwrap();

        assert_eq!(avg.len(), times.len(), "avg length should match times");
        assert_eq!(min.len(), times.len(), "min length should match times");
        assert_eq!(max.len(), times.len(), "max length should match times");
        assert_eq!(count.len(), times.len(), "count length should match times");

        // Verify avg is between min and max, and count > 0 for each bucket
        for i in 0..times.len() {
            if let (Some(avg_val), Some(min_val), Some(max_val)) =
                (avg[i].as_f64(), min[i].as_f64(), max[i].as_f64())
            {
                assert!(
                    avg_val >= min_val && avg_val <= max_val,
                    "avg ({avg_val}) should be between min ({min_val}) and max ({max_val}) at bucket {i}"
                );
            }

            let cnt = count[i].as_i64().unwrap_or(0);
            assert!(
                cnt > 0,
                "count should be > 0 for buckets within seeded range (bucket {i}, got {cnt})"
            );
        }

        // Each hour should have ~6 readings (10-min intervals)
        let first_count = count[0].as_i64().unwrap();
        assert!(
            first_count >= 5 && first_count <= 7,
            "should be ~6 readings per hour, got {first_count}"
        );
    }
}

#[tokio::test]
#[serial]
async fn test_aggregates_daily() {
    let (_db, app) = setup().await;
    let site_id = common::SITE1_ID;

    let (status, body) = common::get_json(
        &app,
        &format!(
            "/api/private/sites/{site_id}/aggregates/daily?start=2025-01-15T00:00:00Z&end=2025-01-17T00:00:00Z"
        ),
    )
    .await;

    assert_eq!(status, 200);

    let times = body["times"].as_array().unwrap();
    assert_eq!(
        times.len(),
        2,
        "should have 2 daily buckets for 2-day range"
    );

    let params = body["parameters"].as_array().unwrap();
    for param in params {
        let count = param["count"].as_array().unwrap();
        assert_eq!(count.len(), 2);

        // ~144 readings per day (24h × 6 readings/h)
        let day1_count = count[0].as_i64().unwrap();
        assert!(
            day1_count >= 140 && day1_count <= 148,
            "should be ~144 readings per day, got {day1_count}"
        );
    }
}

#[tokio::test]
#[serial]
async fn test_aggregates_sensor_types_filter() {
    let (_db, app) = setup().await;
    let site_id = common::SITE1_ID;

    let (status, body) = common::get_json(
        &app,
        &format!(
            "/api/private/sites/{site_id}/aggregates/hourly?start=2025-01-15T00:00:00Z&end=2025-01-16T00:00:00Z&sensor_types=Conductivity"
        ),
    )
    .await;

    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 1, "should return only 1 parameter");
    assert_eq!(params[0]["type"].as_str().unwrap(), "Conductivity");
}

#[tokio::test]
#[serial]
async fn test_aggregates_with_alarms() {
    let (_db, app) = setup().await;
    let site_id = common::SITE1_ID;

    let (status, body) = common::get_json(
        &app,
        &format!(
            "/api/private/sites/{site_id}/aggregates/hourly?start=2025-01-15T00:00:00Z&end=2025-01-17T00:00:00Z&alarms=true"
        ),
    )
    .await;

    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    assert!(!params.is_empty());

    let mut found_nonzero = false;
    for param in params {
        assert!(
            param["max_severity"].is_array(),
            "parameter should have max_severity array when alarms=true"
        );

        let max_sev = param["max_severity"].as_array().unwrap();
        let times = body["times"].as_array().unwrap();
        assert_eq!(
            max_sev.len(),
            times.len(),
            "max_severity should match times length"
        );

        for sev in max_sev {
            if !sev.is_null() {
                let s = sev.as_i64().unwrap();
                assert!(
                    s == 0 || s == 1 || s == 2,
                    "max_severity should be 0, 1, or 2, got: {s}"
                );
                if s > 0 {
                    found_nonzero = true;
                }
            }
        }
    }

    assert!(
        found_nonzero,
        "should have at least one non-zero max_severity from seeded alarm values"
    );
}

#[tokio::test]
#[serial]
async fn test_aggregates_invalid_resolution_returns_400() {
    let (_db, app) = setup().await;
    let site_id = common::SITE1_ID;

    let (status, _body) = common::get(
        &app,
        &format!(
            "/api/private/sites/{site_id}/aggregates/minutely?start=2025-01-15T00:00:00Z&end=2025-01-16T00:00:00Z"
        ),
    )
    .await;

    assert!(
        status == 400 || status == 404,
        "invalid resolution should return 400 or 404, got {status}"
    );
}

#[tokio::test]
#[serial]
async fn test_aggregates_missing_params_returns_400() {
    let (_db, app) = setup().await;
    let site_id = common::SITE1_ID;

    // No start or end
    let (status, _body) = common::get(
        &app,
        &format!("/api/private/sites/{site_id}/aggregates/hourly"),
    )
    .await;

    assert_eq!(
        status, 400,
        "aggregates without start/end should return 400"
    );
}

// ============================================================================
// Alarms endpoint: GET /api/private/sites/{site_id}/alarms
// ============================================================================

#[tokio::test]
#[serial]
async fn test_alarms_basic() {
    let (_db, app) = setup().await;
    let site_id = common::SITE1_ID;

    let (status, body) = common::get_json(
        &app,
        &format!(
            "/api/private/sites/{site_id}/alarms?start=2025-01-15T00:00:00Z&end=2025-01-17T00:00:00Z"
        ),
    )
    .await;

    assert_eq!(status, 200);

    // Top-level structure
    assert!(body["project"].is_object(), "should have project object");
    assert!(body["site"].is_object(), "should have site object");
    assert!(body["times"].is_array(), "should have times array");
    assert!(
        body["parameters"].is_array(),
        "should have parameters array"
    );

    let times = body["times"].as_array().unwrap();
    assert!(
        !times.is_empty(),
        "should have violation times from seeded alarm data"
    );

    let params = body["parameters"].as_array().unwrap();
    assert!(!params.is_empty(), "should have parameters with violations");

    for param in params {
        assert!(param["id"].is_string());
        assert!(param["name"].is_string());
        assert!(param["type"].is_string());

        let values = param["values"].as_array().unwrap();
        let severities = param["severities"].as_array().unwrap();

        assert_eq!(values.len(), times.len(), "values should align with times");
        assert_eq!(
            severities.len(),
            times.len(),
            "severities should align with times"
        );

        // Alarms response aligns all parameters to a shared times array.
        // Parameters without a violation at a given time get severity 0.
        // So severities can be 0, 1, or 2.
        for sev in severities {
            if !sev.is_null() {
                let s = sev.as_i64().unwrap();
                assert!(
                    s == 0 || s == 1 || s == 2,
                    "alarm severities should be 0, 1, or 2, got: {s}"
                );
            }
        }
    }

    // The shared times array should only contain times where at least one
    // parameter has a violation — verify at least some exist
    assert!(
        !times.is_empty(),
        "seeded data with threshold-exceeding values should produce violations"
    );
}

#[tokio::test]
#[serial]
async fn test_alarms_severity_filter() {
    let (_db, app) = setup().await;
    let site_id = common::SITE1_ID;

    // Filter for only alarm-level (severity=2)
    let (status, body) = common::get_json(
        &app,
        &format!(
            "/api/private/sites/{site_id}/alarms?start=2025-01-15T00:00:00Z&end=2025-01-17T00:00:00Z&severity=2"
        ),
    )
    .await;

    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    for param in params {
        let severities = param["severities"].as_array().unwrap();
        for sev in severities {
            if !sev.is_null() {
                let s = sev.as_i64().unwrap();
                assert_eq!(
                    s, 2,
                    "with severity=2 filter, all severities should be 2, got: {s}"
                );
            }
        }
    }
}

#[tokio::test]
#[serial]
async fn test_alarms_sensor_types_filter() {
    let (_db, app) = setup().await;
    let site_id = common::SITE1_ID;

    let (status, body) = common::get_json(
        &app,
        &format!(
            "/api/private/sites/{site_id}/alarms?start=2025-01-15T00:00:00Z&end=2025-01-17T00:00:00Z&sensor_types=DO_Temperature"
        ),
    )
    .await;

    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    assert_eq!(
        params.len(),
        1,
        "should return exactly 1 parameter (DO_Temperature), got {}",
        params.len()
    );
    assert_eq!(params[0]["type"].as_str().unwrap(), "DO_Temperature");
}

#[tokio::test]
#[serial]
async fn test_alarms_missing_params_returns_400() {
    let (_db, app) = setup().await;
    let site_id = common::SITE1_ID;

    // No start or end
    let (status, _body) = common::get(&app, &format!("/api/private/sites/{site_id}/alarms")).await;

    assert_eq!(status, 400, "alarms without start/end should return 400");
}
