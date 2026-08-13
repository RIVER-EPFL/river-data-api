//! Every grab is a collection event, so it gets a `samples` row whether it was measured once or
//! several times. Views that read grabs join through `samples`, so a grab missing a row there is
//! invisible to them.
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
    db.query_one(Statement::from_string(
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
async fn a_grab_measured_once_gets_its_sample_row() {
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
        json["samples_created"], 1,
        "the lone grab is a sample: {json}"
    );

    let (n, mean) = sample_stats(&db, time)
        .await
        .expect("the lone grab has a samples row");
    assert_eq!(n, 1, "the sample counts its single replicate");
    assert!(
        (mean.unwrap() - 12.5).abs() < 1e-9,
        "the mean is the measurement itself"
    );
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
