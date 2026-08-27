//! `/streams/retag` refuses to classify a replicate family away from 'spot'. Samples form only
//! from spot readings, so a family reclassified continuous would keep declaring replicates that
//! can no longer become a sample.
//!
//! Run: cargo test --test data_streams replicate_retag_guard -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

const SOURCE: &str = "retagsrc";
const FAMILY_KEY: &str = "STA:DOC_avg_ppb:reps";
const SINGLE_KEY: &str = "STA:Temp";

async fn setup() -> (DatabaseConnection, axum::Router, String, Uuid, Uuid) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, family) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({
            "source_system": SOURCE,
            "source_key": FAMILY_KEY,
            "measurement_type": "spot",
            "replicates": {
                "source_columns": ["DOC_1_ppb", "DOC_2_ppb", "DOC_3_ppb"],
                "portal_mean_column": "DOC_avg_ppb",
            },
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "register family: {family}");
    let family: Uuid = crate::common::e2e::id_of(&family).parse().unwrap();

    let (status, single) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({"source_system": SOURCE, "source_key": SINGLE_KEY, "measurement_type": "spot"}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "register single: {single}");
    let single: Uuid = crate::common::e2e::id_of(&single).parse().unwrap();

    (db, app, token, family, single)
}

async fn measurement_type_of(db: &DatabaseConnection, stream: Uuid) -> Option<String> {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT measurement_type FROM data_streams WHERE id = '{stream}'"),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<Option<String>>("", "measurement_type")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn retagging_a_family_away_from_spot_is_refused() {
    let (db, app, token, family, _single) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/streams/retag",
        &json!({"stream_ids": [family], "measurement_type": "continuous"}),
        &token,
    )
    .await;
    assert_eq!(status, 400, "retag ({status}): {body}");
    assert!(body.contains(FAMILY_KEY), "the refusal names it: {body}");
    assert_eq!(
        measurement_type_of(&db, family).await.as_deref(),
        Some("spot")
    );
}

/// A source-wide retag reaches the family through its source_system rather than its id, so the
/// guard has to read the same selection the update would.
#[tokio::test]
#[serial]
async fn a_source_wide_retag_is_refused_whole() {
    let (db, app, token, family, single) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/streams/retag",
        &json!({"source_system": SOURCE, "measurement_type": "continuous"}),
        &token,
    )
    .await;
    assert_eq!(status, 400, "retag ({status}): {body}");
    assert_eq!(
        measurement_type_of(&db, family).await.as_deref(),
        Some("spot")
    );
    assert_eq!(
        measurement_type_of(&db, single).await.as_deref(),
        Some("spot"),
        "the single-column stream in the same scope is untouched"
    );
}

/// Entity CRUD reaches the same column as `/streams/retag`, so it holds the same guard: an
/// update cannot move a family off 'spot', and cannot clear the classification either (NULL
/// would fall through to the owning sensor's data_frequency).
#[tokio::test]
#[serial]
async fn a_crud_update_cannot_move_a_family_off_spot() {
    let (db, app, token, family, single) = setup().await;

    for body in [
        json!({"measurement_type": "continuous"}),
        json!({"measurement_type": null}),
    ] {
        let (status, resp) = crate::common::put_json_with_token(
            &app,
            &format!("/api/data_streams/{family}"),
            &body,
            &token,
        )
        .await;
        assert_eq!(status, 400, "put {body} ({status}): {resp}");
        assert!(resp.contains(FAMILY_KEY), "the refusal names it: {resp}");
    }
    assert_eq!(
        measurement_type_of(&db, family).await.as_deref(),
        Some("spot")
    );

    let (status, resp) = crate::common::put_json_with_token(
        &app,
        &format!("/api/data_streams/{single}"),
        &json!({"measurement_type": "continuous"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "non-family update ({status}): {resp}");
    assert_eq!(
        measurement_type_of(&db, single).await.as_deref(),
        Some("continuous")
    );

    // An update that leaves measurement_type alone still passes on a family stream.
    let (status, resp) = crate::common::put_json_with_token(
        &app,
        &format!("/api/data_streams/{family}"),
        &json!({"source_name": "renamed"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "unrelated family update ({status}): {resp}");
}

/// The sensor-side mirror: a sensor reaching a family stream cannot be classified 'high' by
/// entity CRUD, matching `/sensors/retag_frequency`.
#[tokio::test]
#[serial]
async fn a_crud_update_cannot_classify_a_family_owning_sensor_high() {
    let (db, app, token, family, _single) = setup().await;

    let sensor_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, data_frequency) VALUES ('{sensor_id}', 'lab dev', 'low')"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!("UPDATE data_streams SET sensor_id = '{sensor_id}' WHERE id = '{family}'"),
    )
    .await;

    let (status, resp) = crate::common::put_json_with_token(
        &app,
        &format!("/api/sensors/{sensor_id}"),
        &json!({"data_frequency": "high"}),
        &token,
    )
    .await;
    assert_eq!(status, 400, "sensor update ({status}): {resp}");
    assert!(resp.contains(FAMILY_KEY), "the refusal names it: {resp}");

    let (status, resp) = crate::common::put_json_with_token(
        &app,
        &format!("/api/sensors/{sensor_id}"),
        &json!({"notes": "still low"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "unrelated sensor update ({status}): {resp}");
}

#[tokio::test]
#[serial]
async fn a_stream_with_no_family_still_retags() {
    let (db, app, token, _family, single) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/streams/retag",
        &json!({"stream_ids": [single], "measurement_type": "continuous"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "retag ({status}): {body}");
    assert_eq!(
        measurement_type_of(&db, single).await.as_deref(),
        Some("continuous")
    );
}
