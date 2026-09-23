//! An `/ingest` pass and a pairing of the same stream, run on two connections at once.
//!
//! Run: cargo test --test readings ingest_pairing_race -- --test-threads=1

use std::time::Duration;

use river_db::routes::private::data_streams::{Entity as Streams, flows, service};
use river_db::routes::private::sensors::service::create_sensor_for_stream;
use river_db::routes::private::sync::service::HoldScope;
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, EntityTrait, Statement, TransactionTrait,
};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

const T1: &str = "2025-06-10T08:00:00Z";

async fn scalar(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .expect("row")
    .try_get::<i64>("", "c")
    .unwrap()
}

async fn stored_digest(db: &DatabaseConnection, stream_id: Uuid) -> Option<String> {
    Streams::find_by_id(stream_id)
        .one(db)
        .await
        .unwrap()
        .expect("the stream exists")
        .last_window_digest
}

async fn register_spot_stream(fx: &crate::common::Fixture, key: &str) -> Uuid {
    let (status, stream) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/streams/register",
        &json!({"source_system": "cnet", "source_key": key, "measurement_type": "spot"}),
        &fx.token,
    )
    .await;
    assert!((200..300).contains(&status), "register: {stream}");
    crate::common::e2e::id_of(&stream).parse().unwrap()
}

/// Everything `/streams/{id}/pair` does inside its transaction, left open for the caller to commit.
async fn pair_uncommitted(
    db: &DatabaseConnection,
    stream_id: Uuid,
) -> sea_orm::DatabaseTransaction {
    let txn = db.begin().await.unwrap();
    let slot: Uuid = crate::common::PARAM_S1_TEMP_ID.parse().unwrap();
    let claimed = service::claim_stream(stream_id, slot, chrono::Utc::now().into())
        .exec(&txn)
        .await
        .unwrap()
        .rows_affected;
    assert_eq!(claimed, 1);
    let stream = Streams::find_by_id(stream_id)
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    let ctx = create_sensor_for_stream(
        &txn,
        &stream,
        crate::common::GLOBAL_PARAM_TEMP_ID.parse().unwrap(),
        crate::common::SITE1_ID.parse().unwrap(),
        None,
    )
    .await
    .unwrap();
    flows::backfill(&txn, HoldScope::Stream(stream_id), ctx.deployment_id)
        .await
        .unwrap();
    txn
}

/// Waits until another backend of this database is blocked on a row lock, ie. the ingest has
/// reached the stream row the open pairing holds.
async fn wait_for_lock_waiter(db: &DatabaseConnection) {
    for _ in 0..200 {
        let waiting = scalar(
            db,
            "SELECT count(*) AS c FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .await;
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the ingest never reached the stream row the pairing holds");
}

/// Scenario: a windowed pass on an unpaired spot stream starts while a pairing of that stream is
/// uncommitted, and the pairing commits before the pass does.
/// Expected behaviour: the pass stores its rows attributed to the slot the pairing committed, since
/// the pairing's backfill ran before those rows existed and nothing else attributes them.
#[tokio::test]
#[serial]
async fn a_pass_racing_a_pairing_stores_its_rows_attributed() {
    let fx = crate::common::seeded_app().await;
    let (sync_token, _service) = crate::common::seed_sync_session_token(&fx.db).await;
    let stream_id = register_spot_stream(&fx, "RACE:DOC:reps").await;

    let pairing = pair_uncommitted(&fx.db, stream_id).await;

    let app = fx.app.clone();
    let ingest = tokio::spawn(async move {
        crate::common::post_json_parse_with_token(
            &app,
            "/api/ingest",
            &json!({
                "stream_id": stream_id,
                "window": {
                    "from": "2025-01-01T00:00:00Z",
                    "to": "2026-01-01T00:00:00Z",
                    "source_rows_read": 1,
                    "content_digest": "d1"
                },
                "readings": [
                    {"time": T1, "raw_value": 10.0, "replicate_index": 0},
                    {"time": T1, "raw_value": 12.0, "replicate_index": 1}
                ],
            }),
            &sync_token,
        )
        .await
    });

    wait_for_lock_waiter(&fx.db).await;
    pairing.commit().await.unwrap();
    let (status, body) = ingest.await.unwrap();
    assert_eq!(status, 200, "{body}");

    let site = crate::common::SITE1_ID;
    assert_eq!(
        scalar(
            &fx.db,
            &format!(
                "SELECT count(*) AS c FROM readings \
                 WHERE stream_id = '{stream_id}' AND site_id = '{site}'"
            ),
        )
        .await,
        2,
        "the pass's rows are attributed to the slot the pairing committed: {body}"
    );
}

/// Scenario: a pass attributed its rows against an unpaired stream, and a pairing committed
/// between the pass's commit and its write of the cursor and handshake digest.
/// Expected behaviour: the pairing wins. The digest the pairing cleared stays cleared, so the next
/// pass re-sends the window rather than being skipped as unchanged.
#[tokio::test]
#[serial]
async fn a_pass_state_written_after_a_pairing_is_dropped() {
    let fx = crate::common::seeded_app().await;
    let stream_id = register_spot_stream(&fx, "RACE:DOC:digest").await;
    pair_uncommitted(&fx.db, stream_id)
        .await
        .commit()
        .await
        .unwrap();

    let newest = chrono::DateTime::parse_from_rfc3339(T1).unwrap();
    let written = service::record_pass(stream_id, None, Some(newest), Some("d1".into()))
        .exec(&fx.db)
        .await
        .unwrap()
        .rows_affected;
    assert_eq!(written, 0, "a pass attributed as unpaired writes nothing");
    assert_eq!(stored_digest(&fx.db, stream_id).await, None);

    let slot: Uuid = crate::common::PARAM_S1_TEMP_ID.parse().unwrap();
    let written = service::record_pass(stream_id, Some(slot), Some(newest), Some("d1".into()))
        .exec(&fx.db)
        .await
        .unwrap()
        .rows_affected;
    assert_eq!(
        written, 1,
        "a pass attributed under the committed pairing writes"
    );
    assert_eq!(
        stored_digest(&fx.db, stream_id).await.as_deref(),
        Some("d1")
    );
}
