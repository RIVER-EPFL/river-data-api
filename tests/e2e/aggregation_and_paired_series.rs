//! End-to-end visualization data layer: every continuous-aggregate resolution responds (US-6.2),
//! and a paired-parameter query returns two aligned series for a scatter plot (US-6.4). The chart
//! rendering itself is UI-only; this exercises the data the charts consume.
//!
//! Run: cargo test --test e2e -- --test-threads=1

use crate::common::e2e;
use serial_test::serial;

#[tokio::test]
#[serial]
async fn aggregation_resolutions_and_paired_parameter_series() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let site1 = crate::common::SITE1_ID;
    // The seed has readings on 2025-01-15..18; use a window covering all of January so every
    // resolution's bucket (hour/day/week/month) falls inside the range.
    let start = "2025-01-01T00:00:00Z";
    let end = "2025-02-01T00:00:00Z";

    // US-6.2: each resolution returns a finite average series for a seeded parameter.
    for res in ["hourly", "daily", "weekly", "monthly"] {
        let uri = format!("/api/sites/{site1}/aggregates/{res}?start={start}&end={end}");
        let (status, agg) = crate::common::get_json_with_token(&app, &uri, &token).await;
        assert_eq!(status, 200, "{res} aggregate ({status}): {agg}");
        let avg = e2e::field_for(&agg, crate::common::GLOBAL_PARAM_DO_ID, "avg");
        assert!(
            avg.iter().any(|v| v.is_finite()),
            "{res}: Dissolved_O2 should have a finite average"
        );
    }

    // US-6.4: a two-parameter query returns aligned series (scatter data) and nothing else.
    let uri = format!(
        "/api/sites/{site1}/readings?parameter_ids={},{}&start={start}&end=2025-01-15T01:00:00Z",
        crate::common::GLOBAL_PARAM_DO_ID,
        crate::common::GLOBAL_PARAM_COND_ID
    );
    let (status, readings) = crate::common::get_json_with_token(&app, &uri, &token).await;
    assert_eq!(status, 200, "paired readings ({status}): {readings}");
    let times = readings["times"].as_array().expect("times array").len();
    assert!(times > 0, "expected readings in the window");
    let do_vals = e2e::values_for(&readings, crate::common::GLOBAL_PARAM_DO_ID);
    let cond_vals = e2e::values_for(&readings, crate::common::GLOBAL_PARAM_COND_ID);
    assert_eq!(
        do_vals.len(),
        times,
        "DO series aligns with the shared time axis"
    );
    assert_eq!(
        cond_vals.len(),
        times,
        "Conductivity series aligns with the shared time axis"
    );
    assert_eq!(
        readings["parameters"].as_array().unwrap().len(),
        2,
        "parameter_ids filter should return exactly the two requested parameters: {readings}"
    );
}
