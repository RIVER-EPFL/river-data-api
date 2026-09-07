//! Where a pairing's auto-created deployment opens.
//!
//! Expected behaviour: a stream paired months after it started measuring gets a deployment opening
//! at its first reading, so the history it already holds is covered rather than left with no
//! deployment. A slot whose previous instrument was recalled clamps the new deployment to that
//! recall, because the earlier instrument owns the history it covered.

use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::sensor_lifecycle::create_unpaired_stream_with_device;
use crate::common::{GLOBAL_PARAM_TEMP_ID, PARAM_S1_TEMP_ID, SITE1_ID};

async fn setup() -> (axum::Router, String, sea_orm::DatabaseConnection) {
    let f = crate::common::seeded_app().await;
    (f.app, f.token, f.db)
}

async fn seed_readings(db: &sea_orm::DatabaseConnection, stream_id: Uuid, times: &[&str]) {
    for (i, t) in times.iter().enumerate() {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO readings (stream_id, time, raw_value, replicate_index) \
                 VALUES ('{stream_id}', '{t}', {}, 0)",
                10.0 + i as f64
            ),
        )
        .await;
    }
}

async fn pair(app: &axum::Router, token: &str, stream_id: Uuid) {
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({ "site_parameter_id": PARAM_S1_TEMP_ID }),
        token,
    )
    .await;
    assert!((200..300).contains(&status), "pair ({status}): {body}");
}

async fn open_deployment(
    db: &sea_orm::DatabaseConnection,
) -> (Uuid, chrono::DateTime<chrono::FixedOffset>) {
    use sea_orm::{ConnectionTrait, Statement};
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT id, deployed_from FROM sensor_deployments \
                 WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}' \
                   AND deployed_until IS NULL"
            ),
        ))
        .await
        .expect("query the open deployment")
        .expect("the pairing opened one");
    (
        row.try_get("", "id").unwrap(),
        row.try_get("", "deployed_from").unwrap(),
    )
}

#[tokio::test]
#[serial]
async fn a_pairing_opens_the_deployment_at_the_stream_s_first_reading() {
    let (app, token, db) = setup().await;

    let stream = create_unpaired_stream_with_device(&db, "backdated", "SB1-BACKDATE").await;
    seed_readings(
        &db,
        stream,
        &["2024-03-01T00:00:00Z", "2024-03-02T00:00:00Z"],
    )
    .await;

    pair(&app, &token, stream).await;

    let (deployment_id, deployed_from) = open_deployment(&db).await;
    assert_eq!(
        deployed_from.to_rfc3339(),
        "2024-03-01T00:00:00+00:00",
        "the deployment opens where the stream's history starts"
    );

    assert!(
        crate::common::e2e::wait_for_jobs_by_trigger(&db, "pairing_backfill", 30).await,
        "the pairing backfill completes"
    );
    let rows = crate::common::sensor_lifecycle::get_readings(&db, stream).await;
    assert_eq!(rows.len(), 2);
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(
            r.deployment_id,
            Some(deployment_id),
            "reading[{i}] is covered by the deployment"
        );
    }
}

#[tokio::test]
#[serial]
async fn a_recalled_instrument_keeps_the_history_it_covered() {
    let (app, token, db) = setup().await;

    let earlier = crate::common::sensor_lifecycle::create_sensor(&db, "earlier", GLOBAL_PARAM_TEMP_ID).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensor_deployments \
                 (id, sensor_id, site_id, parameter_id, deployed_from, deployed_until, deployment_type) \
             VALUES (gen_random_uuid(), '{}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', \
                     '2024-01-01T00:00:00Z', '2024-06-01T00:00:00Z', 'permanent')",
            earlier.id
        ),
    )
    .await;

    let stream = create_unpaired_stream_with_device(&db, "successor", "SB1-SUCCESSOR").await;
    seed_readings(&db, stream, &["2024-03-01T00:00:00Z"]).await;

    pair(&app, &token, stream).await;

    let (_, deployed_from) = open_deployment(&db).await;
    assert_eq!(
        deployed_from.to_rfc3339(),
        "2024-06-01T00:00:00+00:00",
        "the new deployment opens where the recalled one ended, not over it"
    );
}
