//! Unpairing one stream of a multi-channel instrument.
//!
//! Expected behaviour: a logger deployed at one site for several parameters holds one open
//! deployment per (site, parameter). Unpairing one of its streams ends that channel's deployment
//! and leaves the others open, which is the cardinality every other lifecycle path already keeps.

use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::sensor_lifecycle::create_unpaired_stream_with_device;
use crate::common::{PARAM_S1_DO_ID, PARAM_S1_TEMP_ID, SITE1_ID};

async fn setup() -> (axum::Router, String, sea_orm::DatabaseConnection) {
    let f = crate::common::seeded_app().await;
    (f.app, f.token, f.db)
}

async fn open_deployments(db: &sea_orm::DatabaseConnection, site_id: &str) -> Vec<Uuid> {
    use sea_orm::{ConnectionTrait, Statement};
    db.query_all_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT parameter_id FROM sensor_deployments \
             WHERE site_id = '{site_id}' AND deployed_until IS NULL ORDER BY parameter_id"
        ),
    ))
    .await
    .expect("query deployments")
    .iter()
    .map(|r| r.try_get::<Uuid>("", "parameter_id").expect("parameter_id"))
    .collect()
}

async fn pair(app: &axum::Router, token: &str, stream_id: Uuid, site_parameter_id: &str) {
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({ "site_parameter_id": site_parameter_id }),
        token,
    )
    .await;
    assert!((200..300).contains(&status), "pair ({status}): {body}");
}

#[tokio::test]
#[serial]
async fn unpairing_one_channel_leaves_the_instrument_s_other_deployments_open() {
    let (app, token, db) = setup().await;

    let temp_stream = create_unpaired_stream_with_device(&db, "logger-temp", "SB1-LOGGER").await;
    let do_stream = create_unpaired_stream_with_device(&db, "logger-do", "SB1-LOGGER").await;
    pair(&app, &token, temp_stream, PARAM_S1_TEMP_ID).await;
    pair(&app, &token, do_stream, PARAM_S1_DO_ID).await;

    let before = open_deployments(&db, SITE1_ID).await;
    assert_eq!(
        before.len(),
        2,
        "one logger serving two parameters at a site holds two open deployments: {before:?}"
    );

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{temp_stream}/unpair"),
        &json!({}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "unpair ({status}): {body}");

    let after = open_deployments(&db, SITE1_ID).await;
    let expected: Uuid = crate::common::GLOBAL_PARAM_DO_ID
        .parse()
        .expect("param uuid");
    assert_eq!(
        after,
        vec![expected],
        "unpairing the temperature stream ends that channel only; the dissolved-oxygen \
         deployment of the same logger stays open"
    );
}
