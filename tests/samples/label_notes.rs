//! A grab's label, notes and author are properties of the measurement, so they are stored on each
//! reading the request lands. The `samples` row is statistics only, and a single reading forms
//! none.
//!
//! Run with: cargo test --test samples label_notes

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

const GRAB_TIME: &str = "2025-02-10T09:00:00Z";

/// The label, notes and author stored on the group's readings, plus how many readings carry them.
async fn reading_facts(
    db: &DatabaseConnection,
) -> Option<(Option<String>, Option<String>, Option<String>, i64)> {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT MIN(label) AS label, MIN(notes) AS notes, MIN(created_by) AS created_by, \
                    COUNT(*)::bigint AS n \
             FROM readings \
             WHERE site_id = '{}' AND parameter_id = '{}' AND time = '{GRAB_TIME}'",
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
            row.try_get::<Option<String>>("", "created_by").unwrap(),
            row.try_get::<i64>("", "n").unwrap(),
        )
    })
}

async fn caller(db: &DatabaseConnection) -> String {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT 'token:' || id::text AS caller FROM api_tokens",
    ))
    .await
    .unwrap()
    .expect("the seeded token")
    .try_get("", "caller")
    .unwrap()
}

async fn sample_count(db: &DatabaseConnection) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT COUNT(*)::bigint AS n FROM samples \
             WHERE site_id = '{}' AND parameter_id = '{}' AND collected_at = '{GRAB_TIME}'",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
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
        "label": label,
        "notes": notes,
        "readings": readings,
    })
}

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

#[tokio::test]
#[serial]
async fn label_and_notes_land_on_every_replicate() {
    let (db, app, token) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &payload(&[10.0, 12.0], Some("batch 7"), Some("filtered on site")),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab insert ({status}): {body}");

    let (label, notes, created_by, n) = reading_facts(&db).await.expect("the readings land");
    assert_eq!(label.as_deref(), Some("batch 7"));
    assert_eq!(notes.as_deref(), Some("filtered on site"));
    assert_eq!(created_by, Some(caller(&db).await));
    assert_eq!(n, 2, "both replicates carry the record");

    let mut repost = payload(&[10.0, 12.0], None, Some("corrected note"));
    repost["mode"] = serde_json::json!("replace");
    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &repost, &token).await;
    assert_eq!(status, 200, "replace re-post ({status}): {body}");

    let (label, notes, _, _) = reading_facts(&db).await.expect("the rewritten readings");
    assert_eq!(
        label.as_deref(),
        Some("batch 7"),
        "a rewrite that says nothing about the label keeps it"
    );
    assert_eq!(
        notes.as_deref(),
        Some("corrected note"),
        "the rewritten group takes the new note"
    );
}

#[tokio::test]
#[serial]
async fn a_single_reading_keeps_its_note_without_a_sample() {
    let (db, app, token) = setup().await;

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
        resp["samples_created"], 0,
        "one measurement is not a group: {resp}"
    );

    let (label, notes, created_by, n) = reading_facts(&db).await.expect("the reading lands");
    assert_eq!(label, None);
    assert_eq!(notes.as_deref(), Some("lone value with context"));
    assert_eq!(created_by, Some(caller(&db).await));
    assert_eq!(n, 1);
    assert_eq!(sample_count(&db).await, 0, "no statistics row is minted");
}
