//! Grab-sample label/notes are stamped onto the sample rows the request creates or reuses, and a
//! single-reading grab gets an n=1 sample whether or not it carries a note.
//!
//! Run with: cargo test --test samples label_notes

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

const GRAB_TIME: &str = "2025-02-10T09:00:00Z";

async fn sample_row(db: &DatabaseConnection) -> Option<(Option<String>, Option<String>, i32)> {
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT label, notes, n FROM samples \
             WHERE site_id = '{}' AND parameter_id = '{}' AND collected_at = '{GRAB_TIME}'",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    ))
    .await
    .unwrap()
    .map(|row| {
        (
            row.try_get::<Option<String>>("", "label").unwrap(),
            row.try_get::<Option<String>>("", "notes").unwrap(),
            row.try_get::<i32>("", "n").unwrap(),
        )
    })
}

fn payload(values: &[f64], label: Option<&str>, notes: Option<&str>) -> serde_json::Value {
    let readings: Vec<serde_json::Value> = values
        .iter()
        .map(|v| {
            serde_json::json!({
                "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
                "value": v,
                "time": GRAB_TIME,
            })
        })
        .collect();
    serde_json::json!({
        "site_id": crate::common::SITE1_ID,
        "created_by": "test",
        "label": label,
        "notes": notes,
        "readings": readings,
    })
}

#[tokio::test]
#[serial]
async fn label_and_notes_land_on_replicate_sample() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &payload(&[10.0, 12.0], Some("batch 7"), Some("filtered on site")),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab insert ({status}): {body}");

    let (label, notes, n) = sample_row(&db).await.expect("sample row created");
    assert_eq!(label.as_deref(), Some("batch 7"));
    assert_eq!(notes.as_deref(), Some("filtered on site"));
    assert_eq!(n, 2);

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &payload(&[10.0, 12.0], None, Some("corrected note")),
        &token,
    )
    .await;
    assert_eq!(status, 200, "re-post ({status}): {body}");

    let (label, notes, _) = sample_row(&db).await.expect("sample row reused");
    assert_eq!(
        label.as_deref(),
        Some("batch 7"),
        "unset fields keep old value"
    );
    assert_eq!(
        notes.as_deref(),
        Some("corrected note"),
        "reused sample takes new note"
    );
}

#[tokio::test]
#[serial]
async fn single_reading_with_note_still_creates_sample() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &payload(&[42.0], None, Some("lone value with context")),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab insert ({status}): {body}");
    let resp: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        resp["samples_created"], 1,
        "n=1 group still gets a sample: {resp}"
    );

    let (label, notes, n) = sample_row(&db)
        .await
        .expect("sample row created for single reading");
    assert_eq!(label, None);
    assert_eq!(notes.as_deref(), Some("lone value with context"));
    assert_eq!(n, 1, "trigger counts the single reading");
}

/// A grab is a collection event from its first measurement, so a bare single reading gets its
/// sample row too: the views that read grabs join through `samples`, and one without a row there
/// would be invisible to them.
#[tokio::test]
#[serial]
async fn single_reading_without_note_still_creates_sample() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &payload(&[42.0], None, None),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab insert ({status}): {body}");

    let (label, notes, n) = sample_row(&db)
        .await
        .expect("sample row created for a bare single grab");
    assert_eq!(label, None);
    assert_eq!(notes, None);
    assert_eq!(n, 1, "trigger counts the single reading");
}
