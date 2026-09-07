//! An intern's entry is pending, and the public API does not publish a pending measurement (Q18,
//! Q21). The row is absent from the served instant, not served with a status column, and a
//! manager's verify is what puts it on the public arm. The private arm is the other half of that
//! decision: it serves the entry and says it is pending, because it is the only surface that can.
//!
//! Run: cargo test --test public_api unverified_entries -- --test-threads=1

use sea_orm::DatabaseConnection;
use serial_test::serial;
use uuid::Uuid;

const SPOT_TIME: &str = "2025-01-15T00:05:30Z";
const WINDOW: &str = "start=2025-01-15T00:00:00Z&end=2025-01-15T01:00:00Z";
const READINGS_URI: &str = "/api/public/test-river/sites/upstream/readings";

/// A public project with one exposed parameter and one two-replicate spot group.
async fn setup() -> (DatabaseConnection, axum::Router, Uuid) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    for sql in [
        format!(
            "UPDATE projects SET is_public = true, public_code = 'test-river' WHERE id = '{}'",
            crate::common::PROJECT_ID
        ),
        format!(
            "UPDATE sites SET public_code = 'upstream' WHERE id = '{}'",
            crate::common::SITE1_ID
        ),
        format!(
            "UPDATE site_parameters SET is_public = true WHERE id = '{}'",
            crate::common::PARAM_S1_TEMP_ID
        ),
    ] {
        crate::common::exec(&db, &sql).await;
    }

    let stream_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active) \
             VALUES ('{stream_id}', 'pendingsrc', '{}', true)",
            Uuid::new_v4()
        ),
    )
    .await;
    for (idx, value) in [(0i16, 10.0f64), (1, 20.0)] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                    raw_value, measurement_type) \
                 VALUES ('{stream_id}', '{site}', '{param}', '{SPOT_TIME}', {idx}, {value}, 'spot')",
                site = crate::common::SITE1_ID,
                param = crate::common::GLOBAL_PARAM_TEMP_ID,
            ),
        )
        .await;
    }

    let app = crate::common::build_test_app(db.clone());
    (db, app, stream_id)
}

async fn set_unverified(db: &DatabaseConnection, stream: Uuid, index: Option<i16>, pending: bool) {
    let clause = index.map_or_else(String::new, |i| format!(" AND replicate_index = {i}"));
    crate::common::exec(
        db,
        &format!(
            "UPDATE readings SET unverified = {pending} \
             WHERE stream_id = '{stream}' AND time = '{SPOT_TIME}'{clause}"
        ),
    )
    .await;
}

/// The value the public endpoint serves for the instant, or `None` when it serves the instant not
/// at all.
async fn served(app: &axum::Router) -> Option<f64> {
    let (status, body) = crate::common::get_json(
        app,
        &format!("{READINGS_URI}?{WINDOW}&measurement_type=spot"),
    )
    .await;
    assert_eq!(status, 200, "spot readings ({status}): {body}");
    assert!(
        !body.to_string().contains("unverified"),
        "the pending state is not a public column: {body}"
    );
    let values = body["parameters"][0]["values"].as_array()?;
    values.first().and_then(serde_json::Value::as_f64)
}

#[tokio::test]
#[serial]
async fn a_pending_entry_is_not_published_and_a_verify_publishes_it() {
    let (db, app, stream) = setup().await;
    assert!(
        (served(&app).await.expect("the entry is served") - 10.0).abs() < 1e-9,
        "the verified group serves its lowest replicate"
    );

    // One pending replicate leaves the instant standing on the rest.
    set_unverified(&db, stream, Some(0), true).await;
    assert!(
        (served(&app).await.expect("the instant still serves") - 20.0).abs() < 1e-9,
        "the pending replicate is skipped, not the instant"
    );

    // A wholly pending entry is absent.
    set_unverified(&db, stream, None, true).await;
    assert_eq!(served(&app).await, None, "nothing pending is published");

    set_unverified(&db, stream, None, false).await;
    assert!(
        (served(&app).await.expect("verified") - 10.0).abs() < 1e-9,
        "a verify puts the entry on the public arm"
    );
}

/// Scenario: the same pending entry, read through the private site readings arm.
/// Expected behaviour: it is served, and it is marked pending. The public arm, the alarms and the
/// seasonal check all leave such a reading out, so a manager who cannot see it here has no way to
/// know there is anything to countersign.
#[tokio::test]
#[serial]
async fn the_private_arm_serves_a_pending_entry_and_says_it_is_pending() {
    let (db, app, stream) = setup().await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;

    let flags = |body: &serde_json::Value| -> Vec<Option<bool>> {
        body["parameters"]
            .as_array()
            .expect("parameters")
            .iter()
            .find(|p| p["parameter_id"] == crate::common::GLOBAL_PARAM_TEMP_ID)
            .expect("the temperature slot")["unverified"]
            .as_array()
            .expect("unverified is always served on the private arm")
            .iter()
            .map(serde_json::Value::as_bool)
            .collect()
    };

    let uri = format!(
        "/api/sites/{}/readings?{WINDOW}&measurement_type=spot",
        crate::common::SITE1_ID
    );

    let (status, body) = crate::common::get_json_with_token(&app, &uri, &token).await;
    assert_eq!(status, 200, "private readings ({status}): {body}");
    assert!(
        flags(&body).iter().all(|f| f != &Some(true)),
        "nothing is pending yet: {body}"
    );

    set_unverified(&db, stream, None, true).await;

    let (status, body) = crate::common::get_json_with_token(&app, &uri, &token).await;
    assert_eq!(status, 200, "private readings ({status}): {body}");
    let values = body["parameters"]
        .as_array()
        .expect("parameters")
        .iter()
        .find(|p| p["parameter_id"] == crate::common::GLOBAL_PARAM_TEMP_ID)
        .expect("the temperature slot")["values"]
        .as_array()
        .expect("values")
        .iter()
        .filter(|v| !v.is_null())
        .count();
    assert!(values > 0, "the private arm still serves the entry: {body}");
    assert!(
        flags(&body).contains(&Some(true)),
        "and marks it pending, which the public arm never does: {body}"
    );

    // The public arm's answer to the same state, so the two halves are asserted together.
    assert_eq!(served(&app).await, None, "nothing pending is published");
}
