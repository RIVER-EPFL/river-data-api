//! `GET /readings`, the generated list: the rows for one slot, filtered and paged, with no custom
//! handler behind it.
//!
//! Run: cargo test --test readings entity_list -- --test-threads=1

use serde_json::json;
use serial_test::serial;

use crate::common::e2e::percent_encode;
use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const T: &str = "2025-06-01T10:00:00Z";

async fn setup() -> (axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let readings: Vec<_> = [10.0, 20.0, 30.0]
        .iter()
        .enumerate()
        .map(|(i, v)| {
            json!({ "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": v, "time": T, "replicate_index": i })
        })
        .collect();
    let (status, body) = crate::common::post_checked_grab_parse(
        &app,
        &json!({ "site_id": SITE1_ID, "readings": readings }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab save: {body}");
    (app, token)
}

async fn list(app: &axum::Router, token: &str, query: &str) -> (u16, serde_json::Value) {
    crate::common::get_json_with_token(app, &format!("/api/readings?{query}"), token).await
}

#[tokio::test]
#[serial]
async fn the_slot_filter_answers_the_rows_of_one_instant() {
    let (app, token) = setup().await;

    let filter = percent_encode(&format!(
        r#"{{"site_id":"{SITE1_ID}","parameter_id":"{GLOBAL_PARAM_TEMP_ID}","time":"{T}"}}"#
    ));
    let (status, body) = list(&app, &token, &format!("filter={filter}")).await;
    assert_eq!(status, 200, "{body}");
    let rows = body.as_array().expect("a list of readings");
    assert_eq!(rows.len(), 3, "the three replicates of the grab: {body}");
    assert!(
        rows.iter()
            .all(|r| r["site_id"] == SITE1_ID && r["time"] == T),
        "every row is the instant's: {body}"
    );

    // An instant with no readings answers an empty list, not the whole table.
    let empty = percent_encode(&format!(
        r#"{{"parameter_id":"{GLOBAL_PARAM_TEMP_ID}","time":"2025-06-02T10:00:00Z"}}"#
    ));
    let (status, body) = list(&app, &token, &format!("filter={empty}")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.as_array().map(Vec::len), Some(0), "{body}");
}

#[tokio::test]
#[serial]
async fn the_list_pages_by_range() {
    let (app, token) = setup().await;

    let (status, body) = list(
        &app,
        &token,
        "range=%5B0%2C1%5D&sort=%5B%22time%22%2C%22ASC%22%5D",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body.as_array().map(Vec::len),
        Some(2),
        "the range is the page size: {body}"
    );
}

#[tokio::test]
#[serial]
async fn the_list_names_what_each_row_carries_by_id() {
    let (app, token) = setup().await;

    let filter = percent_encode(&format!(
        r#"{{"site_id":"{SITE1_ID}","parameter_id":"{GLOBAL_PARAM_TEMP_ID}","time":"{T}"}}"#
    ));
    let (status, body) = list(&app, &token, &format!("filter={filter}")).await;
    assert_eq!(status, 200, "{body}");
    let row = &body.as_array().expect("a list of readings")[0];
    assert_eq!(row["site_name"], "Upstream Station", "{row}");
    assert_eq!(row["parameter_code"], "DO_Temperature", "{row}");
    assert_eq!(row["units"], "°C", "{row}");
    assert_eq!(row["source_system"], "grab_sample", "{row}");
    assert!(row["source_key"].is_string(), "{row}");
    assert!(
        row["calibration"].is_null(),
        "an uncorrected value names no calibration: {row}"
    );
    assert!(row["curve"].is_null(), "{row}");
}
