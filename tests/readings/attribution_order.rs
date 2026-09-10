//! Which instrument a write path attributes a reading to when the row names none.
//!
//! Two orders are in the tree and both are deliberate. `/readings/batch` and the CSV importer
//! prefer the instrument deployed at the slot over the channel's own, because the slot's
//! deployment is a physical fact about what was in the water. `/ingest` prefers the stream's
//! frozen instrument, because a sync channel's readings come from that channel's device and the
//! slot timeline is only its fallback. The trigger fills the column from the stream either way, so
//! a regression in either resolution stores a plausible row and fails nothing: these tests assert
//! which instrument was chosen, not that one was.
//!
//! Run: cargo test --test readings attribution_order -- --test-threads=1

use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::sensor_lifecycle::{create_sensor_without_curve, deploy_sensor_for_parameter};
use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const AT: &str = "2025-07-02T09:00:00Z";

struct Fixture {
    app: axum::Router,
    db: DatabaseConnection,
    token: String,
    /// The instrument the stream carries.
    channel: Uuid,
    /// The instrument deployed at the slot over the reading's time.
    deployed: Uuid,
}

async fn setup() -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let channel = create_sensor_without_curve(&db, "Channel instrument").await;
    let deployed = create_sensor_without_curve(&db, "Deployed instrument").await;
    deploy_sensor_for_parameter(
        &db,
        deployed,
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        "2000-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap(),
    )
    .await;
    Fixture {
        app: crate::common::build_test_app(db.clone()),
        db,
        token,
        channel,
        deployed,
    }
}

/// A stream carrying its own instrument, paired to the seeded slot so writes are attributed.
async fn paired_stream(fx: &Fixture, source_key: &str) -> Uuid {
    let id = Uuid::new_v4();
    fx.db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO data_streams (id, source_system, source_key, is_active, sensor_id, \
                                       site_parameter_id) \
             VALUES ($1, 'attribution-order', $2, true, $3, \
                     (SELECT id FROM site_parameters \
                      WHERE site_id = $4::uuid AND parameter_id = $5::uuid))",
            [
                id.into(),
                source_key.into(),
                fx.channel.into(),
                SITE1_ID.into(),
                GLOBAL_PARAM_TEMP_ID.into(),
            ],
        ))
        .await
        .expect("create stream");
    id
}

async fn stored_instrument(db: &DatabaseConnection, stream_id: Uuid) -> Option<Uuid> {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT sensor_id FROM readings WHERE stream_id = '{stream_id}' AND time = '{AT}'"),
    ))
    .await
    .expect("query readings")
    .expect("the reading is stored")
    .try_get::<Option<Uuid>>("", "sensor_id")
    .unwrap()
}

/// `/readings/batch` mints its own `api` stream, so the comparison is the deployed instrument
/// against the one the batch's stream carries.
#[tokio::test]
#[serial]
async fn batch_attributes_a_row_to_the_deployed_instrument_over_the_channels_own() {
    let fx = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        "/api/readings/batch",
        &json!({ "readings": [{
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "time": AT,
            "raw_value": 1.0,
        }]}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "batch ({status}): {body}");

    let stored = fx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT sensor_id FROM readings \
                 WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}' \
                   AND time = '{AT}'"
            ),
        ))
        .await
        .unwrap()
        .expect("the reading is stored")
        .try_get::<Option<Uuid>>("", "sensor_id")
        .unwrap();
    assert_eq!(
        stored,
        Some(fx.deployed),
        "the deployment covering the time owns the row"
    );

    crate::common::cleanup_test_db(&fx.db).await;
}

/// A row that names its own instrument outranks both, on every path.
#[tokio::test]
#[serial]
async fn a_row_naming_its_own_instrument_outranks_the_deployment_and_the_channel() {
    let fx = setup().await;
    let named = create_sensor_without_curve(&fx.db, "Named on the row").await;

    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        "/api/readings/batch",
        &json!({ "readings": [{
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "time": AT,
            "raw_value": 1.0,
            "sensor_id": named,
        }]}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "batch ({status}): {body}");

    let stored = fx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT sensor_id FROM readings \
                 WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}' \
                   AND time = '{AT}'"
            ),
        ))
        .await
        .unwrap()
        .expect("the reading is stored")
        .try_get::<Option<Uuid>>("", "sensor_id")
        .unwrap();
    assert_eq!(stored, Some(named), "the row's own instrument wins");

    crate::common::cleanup_test_db(&fx.db).await;
}

/// `/ingest` is the other order: the stream's frozen instrument is the owner, and the slot
/// timeline is consulted only when the stream carries none.
#[tokio::test]
#[serial]
async fn ingest_attributes_a_row_to_the_streams_instrument_and_falls_back_to_the_slot() {
    let fx = setup().await;
    let carrying = paired_stream(&fx, "ingest-with-instrument").await;

    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        "/api/ingest",
        &json!({
            "stream_id": carrying,
            "readings": [{ "time": AT, "raw_value": 1.0 }],
        }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "ingest ({status}): {body}");
    assert_eq!(
        stored_instrument(&fx.db, carrying).await,
        Some(fx.channel),
        "the channel's own instrument owns a sync row"
    );

    // A stream with no instrument of its own: the slot's deployment is the fallback. The column
    // default the harness sets would hide this, so the insert names NULL explicitly.
    let bare = Uuid::new_v4();
    fx.db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO data_streams (id, source_system, source_key, is_active, sensor_id, \
                                       site_parameter_id) \
             VALUES ($1, 'attribution-order', 'ingest-bare', true, NULL, \
                     (SELECT id FROM site_parameters \
                      WHERE site_id = $2::uuid AND parameter_id = $3::uuid))",
            [bare.into(), SITE1_ID.into(), GLOBAL_PARAM_TEMP_ID.into()],
        ))
        .await
        .expect("create bare stream");

    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        "/api/ingest",
        &json!({
            "stream_id": bare,
            "readings": [{ "time": AT, "raw_value": 2.0 }],
        }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "ingest on a bare stream ({status}): {body}");
    assert_eq!(
        stored_instrument(&fx.db, bare).await,
        Some(fx.deployed),
        "with no instrument on the channel the deployment covering the time owns the row"
    );

    crate::common::cleanup_test_db(&fx.db).await;
}

/// Expected behaviour: a reading on an unpaired stream is staged, so it names no instrument. The
/// plan has not said which instrument measured it, and a minted channel default is not an answer:
/// the pairing backfill is what stamps site, parameter and instrument together.
#[tokio::test]
#[serial]
async fn an_unpaired_stream_stages_its_readings_rather_than_attributing_them() {
    let fx = setup().await;
    let unpaired = Uuid::new_v4();
    fx.db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO data_streams (id, source_system, source_key, is_active, sensor_id, \
                                       site_parameter_id) \
             VALUES ($1, 'attribution-order', 'ingest-unpaired', true, $2, NULL)",
            [unpaired.into(), fx.channel.into()],
        ))
        .await
        .expect("create unpaired stream");

    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        "/api/ingest",
        &json!({
            "stream_id": unpaired,
            "readings": [{ "time": AT, "raw_value": 3.0 }],
        }),
        &fx.token,
    )
    .await;
    assert_eq!(
        status, 200,
        "ingest on an unpaired stream ({status}): {body}"
    );

    let row = fx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT sensor_id, site_id, parameter_id, calibration_id, deployment_id \
                   FROM readings WHERE stream_id = '{unpaired}' AND time = '{AT}'"
            ),
        ))
        .await
        .expect("query readings")
        .expect("the reading is stored");
    for column in [
        "sensor_id",
        "site_id",
        "parameter_id",
        "calibration_id",
        "deployment_id",
    ] {
        assert_eq!(
            row.try_get::<Option<Uuid>>("", column).unwrap(),
            None,
            "a staged reading names no {column}"
        );
    }

    // Pairing is what attributes it, and the instrument arrives with the site and the parameter.
    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        &format!("/api/streams/{unpaired}/pair"),
        &json!({ "site_parameter_id": crate::common::PARAM_S1_TEMP_ID }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "pair ({status}): {body}");
    assert_eq!(
        stored_instrument(&fx.db, unpaired).await,
        Some(fx.channel),
        "the backfill stamps the instrument the pairing settled on"
    );

    crate::common::cleanup_test_db(&fx.db).await;
}

/// The CSV importer writes to the slot's own stream, so its comparison is the deployed instrument
/// against the one that stream carries.
#[tokio::test]
#[serial]
async fn csv_import_attributes_rows_to_the_deployed_instrument() {
    let fx = setup().await;

    let (status, resp) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/readings/import_csv",
        &json!({
            "site": SITE1_ID,
            "csv": "DateTime,DO_Temperature\n2025-07-02 09:00:00,11.5\n",
        }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let stored = loop {
        let row = fx
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "SELECT sensor_id FROM readings \
                     WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}' \
                       AND time = '{AT}'"
                ),
            ))
            .await
            .unwrap();
        if let Some(row) = row {
            break row.try_get::<Option<Uuid>>("", "sensor_id").unwrap();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the import worker never stored the row"
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    };
    assert_eq!(
        stored,
        Some(fx.deployed),
        "the deployment covering the time owns an imported row"
    );

    crate::common::cleanup_test_db(&fx.db).await;
}
