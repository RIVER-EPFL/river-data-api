//! What a pairing plan's revert and its site metadata cover: only the plan's own streams.
//!
//! Scenario: a source holds two unpaired streams at one site and one stream an operator already
//! paired by hand; a plan is made over the source, applied and reverted.
//!
//! Expected behaviour: the revert unpairs exactly the two streams the plan paired and counts
//! those, the hand-paired stream stays where it was, and the plan's site metadata is one typed row
//! per site its streams name, with the loggers behind them.
//!
//! Run: cargo test --test sync pairing_plan_scope -- --test-threads=1

use sea_orm::ConnectionTrait;
use serial_test::serial;

use crate::common::e2e::count;
use crate::common::plans::run_plan_action;

const SOURCE: &str = "metalp";

async fn seed_source(db: &sea_orm::DatabaseConnection) -> (Vec<String>, String) {
    let planned: Vec<String> = (0..2).map(|_| uuid::Uuid::new_v4().to_string()).collect();
    for (stream, (parameter, key)) in planned
        .iter()
        .zip([("Conductivity", "k1"), ("Temperature", "k2")])
    {
        crate::common::seed_unpaired_stream_with_hierarchy(
            db,
            stream,
            SOURCE,
            key,
            "METALP",
            "GL1_DN",
            parameter,
            "uS/cm",
            Some((46.1, 7.2, 1800.0)),
            3,
        )
        .await;
    }
    let by_hand = uuid::Uuid::new_v4().to_string();
    crate::common::seed_paired_stream(db, &by_hand, SOURCE, "k3", crate::common::PARAM_S1_TEMP_ID)
        .await;
    (planned, by_hand)
}

async fn create_plan(app: &axum::Router, token: &str) -> String {
    let (status, plan) = crate::common::post_json_parse_with_token(
        app,
        "/api/sync/pairing-plans",
        &serde_json::json!({ "source_system": SOURCE }),
        token,
    )
    .await;
    assert_eq!(status, 200, "create ({status}): {plan}");
    plan["id"].as_str().unwrap().to_string()
}

#[tokio::test]
#[serial]
async fn a_revert_unpairs_only_the_streams_its_plan_paired() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (planned, by_hand) = seed_source(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let plan_id = create_plan(&app, &token).await;

    let applied = run_plan_action(&app, &token, &plan_id, "apply").await;
    assert_eq!(applied["streams_paired"], 2, "{applied}");
    let reverted = run_plan_action(&app, &token, &plan_id, "revert").await;
    assert_eq!(reverted["reverted"], 2, "{reverted}");

    let ids = planned
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM data_streams \
                  WHERE id IN ({ids}) AND site_parameter_id IS NOT NULL"
            ),
        )
        .await,
        0,
        "the plan's streams are unpaired"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM data_streams WHERE id = '{by_hand}' \
                   AND site_parameter_id = '{}'",
                crate::common::PARAM_S1_TEMP_ID
            ),
        )
        .await,
        1,
        "the stream paired by hand stays paired"
    );
}

#[tokio::test]
#[serial]
async fn site_metadata_is_one_typed_row_per_plan_site() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (planned, _) = seed_source(&db).await;
    db.execute_unprepared(&format!(
        "UPDATE data_streams SET metadata = jsonb_set(metadata, '{{device}}', \
         '{{\"logger_serial\": \"CR1000-7\", \"logger_device\": \"CR1000\"}}') \
         WHERE id IN ('{}')",
        planned.join("','")
    ))
    .await
    .expect("stamp the logger");
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let plan_id = create_plan(&app, &token).await;

    let (status, rows) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}/site-metadata"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{rows}");
    let rows = rows.as_array().expect("an array");
    assert_eq!(rows.len(), 1, "two streams at one site: {rows:?}");
    assert_eq!(rows[0]["site_name"], "GL1_DN");
    assert_eq!(rows[0]["latitude"], 46.1);
    assert_eq!(rows[0]["longitude"], 7.2);
    assert_eq!(rows[0]["altitude_m"], 1800.0);
    assert!(rows[0]["glacier_name"].is_null(), "{}", rows[0]);
    assert_eq!(
        rows[0]["devices"],
        serde_json::json!([{ "serial": "CR1000-7", "model": "CR1000", "streams": 2 }]),
        "one logger behind both streams"
    );
}
