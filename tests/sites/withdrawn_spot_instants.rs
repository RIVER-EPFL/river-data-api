//! A retraction is reversible, so it must be reportable.
//!
//! A spot instant every one of whose replicates is withdrawn is served by nothing, and a chart
//! drawn from the readings response alone cannot tell it from a visit never made. The response
//! counts such instants whenever the request covers the spot arm, and serves them, marked, under
//! `include_withdrawn`. A group with one live replicate is not retracted and keeps its mean.
//!
//! Run: cargo test --test sites withdrawn_spot_instants -- --test-threads=1

use serial_test::serial;
use uuid::Uuid;

const RETRACTED: &str = "2025-03-04T08:00:00Z";
const PARTIAL: &str = "2025-03-05T08:00:00Z";

async fn seed_spot_groups(db: &sea_orm::DatabaseConnection) -> Uuid {
    let stream_id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active) \
             VALUES ('{stream_id}', 'grab_sample', '{}', true)",
            Uuid::new_v4()
        ),
    )
    .await;
    let site = crate::common::SITE1_ID;
    let param = crate::common::GLOBAL_PARAM_TEMP_ID;
    for (time, values) in [(RETRACTED, [4.0, 4.4]), (PARTIAL, [6.0, 6.4])] {
        for (index, value) in values.iter().enumerate() {
            crate::common::exec(
                db,
                &format!(
                    "INSERT INTO readings (stream_id, site_id, parameter_id, time, \
                        replicate_index, raw_value, measurement_type) \
                     VALUES ('{stream_id}', '{site}', '{param}', '{time}', {index}, {value}, 'spot')"
                ),
            )
            .await;
        }
    }
    // The whole first group is taken back; the second loses one replicate of two.
    crate::common::exec(
        db,
        &format!(
            "UPDATE readings SET withdrawn_at = now() \
             WHERE site_id = '{site}' AND parameter_id = '{param}' \
               AND (time = '{RETRACTED}' OR (time = '{PARTIAL}' AND replicate_index = 1))"
        ),
    )
    .await;
    stream_id
}

async fn read(app: &axum::Router, token: &str, extra: &str) -> serde_json::Value {
    let url = format!(
        "/api/sites/{}/readings?start=2025-03-01T00:00:00Z&end=2025-03-10T00:00:00Z\
         &measurement_type=spot{extra}",
        crate::common::SITE1_ID
    );
    let (status, body) = crate::common::get_json_with_token(app, &url, token).await;
    assert!((200..300).contains(&status), "readings ({status}): {body}");
    body
}

fn series(body: &serde_json::Value) -> &serde_json::Value {
    body["parameters"]
        .as_array()
        .expect("parameters")
        .iter()
        .find(|p| p["parameter_id"] == crate::common::GLOBAL_PARAM_TEMP_ID)
        .expect("the seeded parameter is in the response")
}

/// Expected behaviour: the retracted instant is counted even though nothing serves it, so a chart
/// can say a visit was taken back without fetching a single retracted point.
#[tokio::test]
#[serial]
async fn a_fully_retracted_instant_is_counted_but_not_served() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    seed_spot_groups(&db).await;

    let body = read(&app, &token, "").await;
    let times = body["times"].as_array().expect("times");
    let served: Vec<&str> = times.iter().filter_map(|t| t.as_str()).collect();
    assert!(
        !served.iter().any(|t| t.starts_with("2025-03-04")),
        "the retracted instant is not served: {served:?}"
    );
    assert!(
        served.iter().any(|t| t.starts_with("2025-03-05")),
        "the instant with a live replicate still is: {served:?}"
    );
    assert_eq!(
        series(&body)["withdrawn_count"],
        1,
        "one instant in the window is retracted in full: {body}"
    );
}

/// Expected behaviour: asked for, a retracted instant comes back marked. The partially retracted
/// group is not marked: it still has a live replicate behind its value.
#[tokio::test]
#[serial]
async fn include_withdrawn_serves_the_instant_and_says_which_one_it_is() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    seed_spot_groups(&db).await;

    let body = read(&app, &token, "&include_withdrawn=true").await;
    let times: Vec<String> = body["times"]
        .as_array()
        .expect("times")
        .iter()
        .map(|t| t.as_str().unwrap_or_default().to_string())
        .collect();
    let param = series(&body);
    let withdrawn = param["withdrawn"].as_array().expect("withdrawn column");
    assert_eq!(withdrawn.len(), times.len(), "one entry per row: {body}");

    let retracted = times
        .iter()
        .position(|t| t.starts_with("2025-03-04"))
        .expect("the retracted instant is served now");
    let partial = times
        .iter()
        .position(|t| t.starts_with("2025-03-05"))
        .expect("the partly retracted instant is served");
    assert_eq!(withdrawn[retracted], true, "marked as retracted: {body}");
    assert_eq!(
        withdrawn[partial], false,
        "a live replicate means the visit stands: {body}"
    );
    assert_eq!(
        param["values"][partial], 6.0,
        "served at the live replicate"
    );
}
