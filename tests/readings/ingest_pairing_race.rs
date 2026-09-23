//! An `/ingest`, `/ingest/status_events` or `/readings/batch` write and a pairing, unpair, plan
//! revert or slot delete of the same stream, run on two connections at once.
//!
//! Run: cargo test --test readings ingest_pairing_race -- --test-threads=1

use std::time::Duration;

use river_db::routes::private::data_streams::{
    ActiveModel as StreamRow, Entity as Streams, SlotScope, flows, service,
};
use river_db::routes::private::sensors::service::create_sensor_for_stream;
use river_db::routes::private::sync::service::HoldScope;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ConnectionTrait, DatabaseBackend, DatabaseConnection,
    EntityTrait, QuerySelect, Statement, TransactionTrait,
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
    let slot = (
        crate::common::PARAM_S1_TEMP_ID.parse().unwrap(),
        crate::common::SITE1_ID.parse().unwrap(),
        crate::common::GLOBAL_PARAM_TEMP_ID.parse().unwrap(),
    );
    pair_to_uncommitted(db, stream_id, slot).await
}

/// [`pair_uncommitted`] to the slot given as (site parameter, site, parameter).
async fn pair_to_uncommitted(
    db: &DatabaseConnection,
    stream_id: Uuid,
    (slot, site_id, parameter_id): (Uuid, Uuid, Uuid),
) -> sea_orm::DatabaseTransaction {
    let txn = db.begin().await.unwrap();
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
    let ctx = create_sensor_for_stream(&txn, &stream, parameter_id, site_id, None)
        .await
        .unwrap();
    flows::backfill(&txn, HoldScope::Stream(stream_id), ctx.deployment_id, None)
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

/// Everything `/streams/{id}/unpair` does to the stream and its rows, left open for the caller to
/// commit.
async fn unpair_uncommitted(
    db: &DatabaseConnection,
    stream_id: Uuid,
) -> sea_orm::DatabaseTransaction {
    let txn = db.begin().await.unwrap();
    let stream = Streams::find_by_id(stream_id)
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    let mut row: StreamRow = stream.into();
    row.site_parameter_id = Set(None);
    row.paired_at = Set(None);
    row.update(&txn).await.unwrap();
    flows::retire_slot(&txn, SlotScope::Stream(stream_id), "test")
        .await
        .unwrap();
    txn
}

/// Waits until the request is blocked on a row lock or has already finished, whichever comes
/// first.
async fn wait_for_lock_waiter_or_finish<T>(
    db: &DatabaseConnection,
    request: &tokio::task::JoinHandle<T>,
) {
    for _ in 0..200 {
        if request.is_finished() {
            return;
        }
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
    panic!("the request neither finished nor waited on a lock");
}

async fn post_batch(app: &axum::Router, token: &str, time: &str) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        app,
        "/api/readings/batch",
        &json!({ "readings": [{
            "site_id": crate::common::SITE1_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_DEPTH_ID,
            "time": time,
            "raw_value": 1.5,
        }]}),
        token,
    )
    .await
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

/// Scenario: a status-event pass on an unpaired stream starts while a pairing of that stream is
/// uncommitted, and the pairing commits before the pass does.
/// Expected behaviour: the pass stores its events attributed to the slot the pairing committed,
/// since the pairing's backfill ran before those events existed and nothing else attributes them.
#[tokio::test]
#[serial]
async fn a_status_event_pass_racing_a_pairing_stores_its_events_attributed() {
    let fx = crate::common::seeded_app().await;
    let (sync_token, _service) = crate::common::seed_sync_session_token(&fx.db).await;
    let stream_id = register_spot_stream(&fx, "RACE:DOC:status").await;

    let pairing = pair_uncommitted(&fx.db, stream_id).await;

    let app = fx.app.clone();
    let ingest = tokio::spawn(async move {
        crate::common::post_json_parse_with_token(
            &app,
            "/api/ingest/status_events",
            &json!({
                "stream_id": stream_id,
                "events": [
                    {"time": T1, "value": "OK"},
                    {"time": "2025-06-10T09:00:00Z", "value": "Unreachable"}
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
                "SELECT count(*) AS c FROM status_events \
                 WHERE stream_id = '{stream_id}' AND site_id = '{site}'"
            ),
        )
        .await,
        2,
        "the pass's events are attributed to the slot the pairing committed: {body}"
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

/// Scenario: a `/readings/batch` write to a slot whose api channel is paired has read that pairing,
/// and an unpair of the channel commits before the batch stores its rows.
/// Expected behaviour: the batch stores its rows unattributed, since the unpair released only the
/// rows that existed when it ran and nothing releases the batch's rows afterwards.
#[tokio::test]
#[serial]
async fn a_batch_racing_an_unpair_stores_its_rows_unattributed() {
    let fx = crate::common::seeded_app().await;
    let (status, body) = post_batch(&fx.app, &fx.token, "2025-06-10T07:00:00Z").await;
    assert_eq!(status, 200, "{body}");
    let site = crate::common::SITE1_ID;
    let parameter = crate::common::GLOBAL_PARAM_DEPTH_ID;
    let stream_id: Uuid = Streams::find()
        .all(&fx.db)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.source_system == "api" && s.source_key == format!("{site}:{parameter}"))
        .expect("the batch opened its api channel")
        .id;

    let sensor =
        crate::common::sensor_lifecycle::create_sensor(&fx.db, "Deployed", parameter).await;
    crate::common::sensor_lifecycle::deploy_sensor_for_parameter(
        &fx.db,
        sensor.id,
        site,
        parameter,
        crate::common::sensor_lifecycle::dt("2025-06-01T00:00:00Z"),
    )
    .await;

    let unpair = unpair_uncommitted(&fx.db, stream_id).await;

    let (app, token) = (fx.app.clone(), fx.token.clone());
    let batch = tokio::spawn(async move { post_batch(&app, &token, T1).await });

    wait_for_lock_waiter_or_finish(&fx.db, &batch).await;
    unpair.commit().await.unwrap();
    let (status, body) = batch.await.unwrap();
    assert_eq!(status, 200, "{body}");

    assert_eq!(
        scalar(
            &fx.db,
            &format!(
                "SELECT count(*) AS c FROM readings \
                 WHERE stream_id = '{stream_id}' AND site_id IS NOT NULL"
            ),
        )
        .await,
        0,
        "no row on the unpaired channel is attributed: {body}"
    );
    assert_eq!(
        scalar(
            &fx.db,
            &format!(
                "SELECT count(*) AS c FROM readings \
                 WHERE stream_id = '{stream_id}' AND time = '{T1}' AND sensor_id IS NULL \
                 AND deployment_id IS NULL AND calibration_id IS NULL \
                 AND calibrated_value IS NULL"
            ),
        )
        .await,
        1,
        "the row names no instrument, deployment or curve the slot's deployment would give it"
    );
}

/// What an `/ingest` pass holds between reading its stream and committing: the stream locked
/// `FOR SHARE` and one row stored attributed to the slot the stream is paired to, by the stream's
/// instrument, left open for the caller to commit.
async fn pass_uncommitted(
    db: &DatabaseConnection,
    stream_id: Uuid,
    (site_id, parameter_id): (Uuid, Uuid),
) -> sea_orm::DatabaseTransaction {
    let txn = db.begin().await.unwrap();
    let stream = Streams::find_by_id(stream_id)
        .lock_shared()
        .one(&txn)
        .await
        .unwrap()
        .expect("the stream exists");
    assert!(
        stream.site_parameter_id.is_some(),
        "the pass reads it paired"
    );
    let sensor_id = stream
        .sensor_id
        .expect("a paired stream names its instrument");
    txn.execute_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "INSERT INTO readings (stream_id, time, raw_value, replicate_index, site_id, \
             parameter_id, sensor_id) \
             VALUES ('{stream_id}', '{T1}', 4.2, 0, '{site_id}', '{parameter_id}', '{sensor_id}')"
        ),
    ))
    .await
    .unwrap();
    txn
}

/// An applied vaisala plan pairing one fresh stream to Site 1 / Temperature.
async fn applied_single_stream_plan(fx: &crate::common::Fixture) -> (Uuid, Uuid) {
    let stream_id = Uuid::new_v4();
    crate::common::exec(
        &fx.db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', 'vaisala', 'race-revert', 'race-revert', true)"
        ),
    )
    .await;
    let entries = json!([{
        "stream_id": stream_id,
        "source_key": "race-revert",
        "source_name": "race-revert",
        "action": "pair",
        "project": { "id": crate::common::PROJECT_ID, "name": "Test Project", "create": false },
        "site": { "id": crate::common::SITE1_ID, "name": "Site 1", "create": false, "latitude": null, "longitude": null, "altitude_m": null },
        "parameter": { "id": crate::common::GLOBAL_PARAM_TEMP_ID, "name": "Temperature", "create": false, "units": "C", "group_key": null, "original_names": [] },
        "confidence": "exact",
        "warnings": [],
        "original_parameter_name": null
    }]);
    let plan_id = Uuid::new_v4();
    crate::common::exec(
        &fx.db,
        &format!(
            "INSERT INTO pairing_plans (id, source_system, status, summary, entries) \
             VALUES ('{plan_id}', 'vaisala', 'draft', '{{}}'::jsonb, '{}'::jsonb)",
            entries.to_string().replace('\'', "''")
        ),
    )
    .await;
    crate::common::plans::acknowledge_plan(&fx.app, &fx.token, &plan_id.to_string()).await;
    let (status, text) = crate::common::post_plan_action_with_token(
        &fx.app,
        &plan_id.to_string(),
        "apply",
        &fx.token,
    )
    .await;
    assert!((200..300).contains(&status), "apply ({status}): {text}");
    let job: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        crate::common::jobs::wait_for_job(&fx.db, job["job_id"].as_str().unwrap()).await,
        "completed"
    );
    (plan_id, stream_id)
}

/// Scenario: a pass on a plan-paired stream has stored its rows attributed and not yet committed
/// when an operator reverts the plan.
/// Expected behaviour: the revert waits for the pass and then unattributes its rows with the rest,
/// so nothing on the stream it unpaired stays attributed to the slot the stream no longer feeds.
#[tokio::test]
#[serial]
async fn a_plan_revert_racing_a_pass_unattributes_the_pass_s_rows() {
    let fx = crate::common::seeded_app().await;
    let (plan_id, stream_id) = applied_single_stream_plan(&fx).await;
    let slot = (
        crate::common::SITE1_ID.parse().unwrap(),
        crate::common::GLOBAL_PARAM_TEMP_ID.parse().unwrap(),
    );

    let pass = pass_uncommitted(&fx.db, stream_id, slot).await;

    let db = fx.db.clone();
    let revert = tokio::spawn(async move {
        river_db::routes::private::sync::service::revert_plan(&db, plan_id, None).await
    });

    wait_for_lock_waiter_or_finish(&fx.db, &revert).await;
    pass.commit().await.unwrap();
    revert.await.unwrap().expect("the revert succeeds");

    assert_eq!(
        stored_slot(&fx.db, stream_id).await,
        None,
        "the revert unpairs the stream"
    );
    assert_eq!(
        scalar(
            &fx.db,
            &format!(
                "SELECT count(*) AS c FROM readings \
                 WHERE stream_id = '{stream_id}' AND site_id IS NOT NULL"
            ),
        )
        .await,
        0,
        "no row on the reverted stream is attributed"
    );
}

async fn stored_slot(db: &DatabaseConnection, stream_id: Uuid) -> Option<Uuid> {
    Streams::find_by_id(stream_id)
        .one(db)
        .await
        .unwrap()
        .expect("the stream exists")
        .site_parameter_id
}

/// Scenario: a pass on the only stream feeding an unmeasured slot has stored its first rows
/// attributed and not yet committed when an operator deletes the slot.
/// Expected behaviour: the delete waits for the pass, then finds the slot measured and is refused
/// as a delete of any measured slot is, so the pass's rows stay attributed to a slot that exists.
#[tokio::test]
#[serial]
async fn a_slot_delete_racing_a_pass_is_refused_once_the_pass_commits() {
    let fx = crate::common::seeded_app().await;
    let site_id: Uuid = crate::common::SITE2_ID.parse().unwrap();
    let parameter_id: Uuid = crate::common::GLOBAL_PARAM_DEPTH_ID.parse().unwrap();
    let slot_id = Uuid::new_v4();
    crate::common::exec(
        &fx.db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, is_active) \
             VALUES ('{slot_id}', '{site_id}', '{parameter_id}', 'Depth', true)"
        ),
    )
    .await;
    let stream_id = register_spot_stream(&fx, "RACE:DEPTH:slot").await;
    pair_to_uncommitted(&fx.db, stream_id, (slot_id, site_id, parameter_id))
        .await
        .commit()
        .await
        .unwrap();

    let pass = pass_uncommitted(&fx.db, stream_id, (site_id, parameter_id)).await;

    let (app, token) = (fx.app.clone(), fx.token.clone());
    let delete = tokio::spawn(async move {
        crate::common::delete_with_token(&app, &format!("/api/site_parameters/{slot_id}"), &token)
            .await
    });

    wait_for_lock_waiter_or_finish(&fx.db, &delete).await;
    pass.commit().await.unwrap();
    let (status, body) = delete.await.unwrap();
    assert_eq!(status, 400, "the slot now holds the pass's rows: {body}");

    assert_eq!(stored_slot(&fx.db, stream_id).await, Some(slot_id));
    assert_eq!(
        scalar(
            &fx.db,
            &format!(
                "SELECT count(*) AS c FROM readings \
                 WHERE stream_id = '{stream_id}' AND site_id = '{site_id}' \
                 AND parameter_id = '{parameter_id}'"
            ),
        )
        .await,
        1,
        "the pass's row stays attributed to the slot"
    );
}
