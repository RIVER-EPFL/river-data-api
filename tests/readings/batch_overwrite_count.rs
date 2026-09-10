//! `POST /readings/batch` in `overwrite` mode reports `overwritten` the way the CSV import does:
//! stored rows whose value the write changed, not every key that was already present.
//!
//! Run: cargo test --test readings batch_overwrite_count -- --test-threads=1

use serde_json::json;
use serial_test::serial;

use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

async fn setup() -> (axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    (crate::common::build_test_app(db.clone()), token)
}

fn rows(values: &[f64]) -> Vec<serde_json::Value> {
    values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            json!({
                "site_id": SITE1_ID,
                "parameter_id": GLOBAL_PARAM_TEMP_ID,
                "time": format!("2025-06-15T{:02}:00:00Z", 10 + i),
                "raw_value": v,
            })
        })
        .collect()
}

async fn post(
    app: &axum::Router,
    token: &str,
    values: &[f64],
    conflict: &str,
) -> serde_json::Value {
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/readings/batch",
        &json!({ "readings": rows(values), "conflict": conflict }),
        token,
    )
    .await;
    assert_eq!(status, 200, "batch ({status}): {body}");
    body
}

#[tokio::test]
#[serial]
async fn overwritten_counts_only_rows_whose_value_changed() {
    let (app, token) = setup().await;

    let first = post(&app, &token, &[1.0, 2.0, 3.0], "skip").await;
    assert_eq!(first["inserted"], 3, "{first}");
    assert_eq!(first["overwritten"], 0, "{first}");

    let same = post(&app, &token, &[1.0, 2.0, 3.0], "overwrite").await;
    assert_eq!(same["inserted"], 0, "{same}");
    assert_eq!(
        same["overwritten"], 0,
        "identical values replace nothing: {same}"
    );

    let one_changed = post(&app, &token, &[1.0, 2.5, 3.0], "overwrite").await;
    assert_eq!(one_changed["inserted"], 0, "{one_changed}");
    assert_eq!(one_changed["overwritten"], 1, "{one_changed}");

    let mixed = post(&app, &token, &[1.0, 2.5, 3.5, 4.0], "overwrite").await;
    assert_eq!(mixed["inserted"], 1, "{mixed}");
    assert_eq!(mixed["overwritten"], 1, "{mixed}");
}
