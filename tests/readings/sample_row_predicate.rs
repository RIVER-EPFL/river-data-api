//! A `samples` row is the statistics of two or more replicates at an instant. A grab measured once
//! is the reading itself: no row is minted for it, and everything reading grabs derives n = 1 from
//! the reading when none exists.
//!
//! Run with: cargo test --test readings sample_row_predicate

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

async fn setup() -> (axum::Router, String, DatabaseConnection) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (app, token, db)
}

/// `(n, mean)` of the sample recorded for the seeded site and temperature parameter at `time`.
async fn sample_stats(db: &DatabaseConnection, time: &str) -> Option<(i32, Option<f64>)> {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT n, mean FROM samples \
             WHERE site_id = '{}' AND parameter_id = '{}' AND collected_at = '{time}'",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    ))
    .await
    .unwrap()
    .map(|row| {
        (
            row.try_get::<i32>("", "n").unwrap(),
            row.try_get::<Option<f64>>("", "mean").unwrap(),
        )
    })
}

fn grab(time: &str, values: &[f64]) -> serde_json::Value {
    let readings: Vec<serde_json::Value> = values
        .iter()
        .map(|v| {
            serde_json::json!({
                "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
                "value": v,
                "time": time,
            })
        })
        .collect();
    serde_json::json!({
        "site_id": crate::common::SITE1_ID,
        "readings": readings,
    })
}

#[tokio::test]
#[serial]
async fn a_grab_measured_once_mints_no_sample_row() {
    let (app, token, db) = setup().await;
    let time = "2025-09-01T08:00:00Z";

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &grab(time, &[12.5]),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab entry ({status}): {body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        json["samples_created"], 0,
        "a single measurement is not a group: {json}"
    );
    assert!(
        sample_stats(&db, time).await.is_none(),
        "no statistics row exists for one reading"
    );

    // A second measurement at the same instant is what makes it a sample.
    let mut rewrite = grab(time, &[12.5, 13.5]);
    rewrite["mode"] = serde_json::json!("replace");
    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &rewrite, &token).await;
    assert_eq!(status, 200, "second replicate ({status}): {body}");
    let (n, mean) = sample_stats(&db, time)
        .await
        .expect("the second replicate forms the sample");
    assert_eq!(n, 2, "the sample covers both replicates");
    assert!(
        (mean.unwrap() - 13.0).abs() < 1e-9,
        "the mean is over both replicates"
    );
}

/// A batch is a write path like any other: two spot replicates at one instant are a group, and the
/// rule that decides so lives in one place.
#[tokio::test]
#[serial]
async fn batched_spot_replicates_form_their_sample() {
    let (app, token, db) = setup().await;
    let time = "2025-09-01T11:00:00Z";

    let reading = |index: i16, raw: f64| {
        serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
            "time": time,
            "raw_value": raw,
            "replicate_index": index,
            "measurement_type": "spot",
        })
    };
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &serde_json::json!({ "readings": [reading(0, 4.0), reading(1, 6.0)] }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "batch insert ({status}): {body}");

    let (n, mean) = sample_stats(&db, time)
        .await
        .expect("the batched group has its samples row");
    assert_eq!(n, 2);
    assert!((mean.unwrap() - 5.0).abs() < 1e-9);
}

#[tokio::test]
#[serial]
async fn replicates_of_one_grab_share_one_sample_row() {
    let (app, token, db) = setup().await;
    let time = "2025-09-01T09:00:00Z";

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &grab(time, &[10.0, 11.0, 12.0]),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab entry ({status}): {body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["inserted"], 3);
    assert_eq!(
        json["samples_created"], 1,
        "three replicates are one sample: {json}"
    );

    let (n, mean) = sample_stats(&db, time)
        .await
        .expect("the group has a samples row");
    assert_eq!(n, 3, "all three replicates count");
    assert!(
        (mean.unwrap() - 11.0).abs() < 1e-9,
        "the mean is over the replicates"
    );
}

#[tokio::test]
#[serial]
async fn flagging_a_replicate_leaves_the_sample_on_the_rest() {
    let (app, token, db) = setup().await;
    let time = "2025-09-01T10:00:00Z";

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &grab(time, &[20.0, 30.0]),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab entry ({status}): {body}");

    let (status, flagged) = crate::common::patch_json_with_token(
        &app,
        "/api/readings/flag",
        &serde_json::json!({
            "readings": [{
                "site_id": crate::common::SITE1_ID,
                "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
                "time": time,
                "replicate_index": 1,
            }],
            "reason": "bottle broke",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "flag one replicate ({status}): {flagged}");

    let (n, mean) = sample_stats(&db, time)
        .await
        .expect("the sample survives its replicate being flagged");
    assert_eq!(n, 1, "the flagged replicate is out of the statistics");
    assert!(
        (mean.unwrap() - 20.0).abs() < 1e-9,
        "only the kept replicate averages"
    );
}
