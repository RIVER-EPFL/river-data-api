//! Signal triggers: a paired slot with no recent data raises a stale-data alert, then a recovery
//! notice once data resumes, deduped through notification_state.
//!
//! Run: cargo test --test notifications -- --test-threads=1

use std::sync::{Arc, Mutex};

use river_db::common::AppState;
use river_db::routes::private::notifications::{
    DeliveryResult, NotificationChannel, OutgoingMessage, triggers,
};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

struct MockChannel {
    sent: Arc<Mutex<Vec<OutgoingMessage>>>,
}

#[async_trait::async_trait]
impl NotificationChannel for MockChannel {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn check_health(&self) -> Result<String, String> {
        Ok("mock healthy".to_string())
    }

    async fn deliver(&self, _state: &AppState, msg: &OutgoingMessage) -> Vec<DeliveryResult> {
        self.sent.lock().unwrap().push(msg.clone());
        vec![DeliveryResult {
            recipient: "mock".to_string(),
            outcome: Ok(()),
        }]
    }
}

async fn turb_stream(db: &DatabaseConnection) -> String {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT id FROM data_streams WHERE site_parameter_id = '{}'",
            crate::common::PARAM_S1_TURB_ID
        ),
    ))
    .await
    .unwrap()
    .expect("seeded turbidity stream")
    .try_get::<uuid::Uuid>("", "id")
    .unwrap()
    .to_string()
}

async fn insert_reading(db: &DatabaseConnection, stream: &str, time_sql: &str) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{stream}', '{site}', '{param}', {time_sql}, 50, 0)",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    )
    .await;
}

async fn stale_state_count(db: &DatabaseConnection) -> i64 {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        "SELECT COUNT(*) AS c FROM notification_state WHERE kind = 'stale_data'".to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "c")
    .unwrap()
}

fn kinds<'a>(msgs: &'a [OutgoingMessage], kind: &str) -> Vec<&'a OutgoingMessage> {
    msgs.iter().filter(|m| m.kind == kind).collect()
}

#[tokio::test]
#[serial]
async fn stale_data_fires_then_recovers() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    // Isolate one slot: clear seeded readings, leave a single stale one (threshold is 6h in tests).
    crate::common::exec(&db, "DELETE FROM readings").await;
    let stream = turb_stream(&db).await;
    insert_reading(&db, &stream, "NOW() - INTERVAL '10 hours'").await;

    let sent = Arc::new(Mutex::new(Vec::new()));
    let channels: Vec<Box<dyn NotificationChannel>> =
        vec![Box::new(MockChannel { sent: sent.clone() })];
    triggers::run(&state, &channels).await;

    {
        let msgs = sent.lock().unwrap();
        let stale = kinds(&msgs, "stale_data");
        assert_eq!(stale.len(), 1, "exactly one slot is stale");
        assert!(stale[0].body.contains("No data"), "body: {}", stale[0].body);
    }
    assert_eq!(stale_state_count(&db).await, 1, "firing state recorded");

    // A second run while still stale must not re-notify.
    sent.lock().unwrap().clear();
    triggers::run(&state, &channels).await;
    assert!(
        kinds(&sent.lock().unwrap(), "stale_data").is_empty(),
        "no re-notify while still stale"
    );

    // Data resumes → recovery notice, state cleared.
    insert_reading(&db, &stream, "NOW()").await;
    sent.lock().unwrap().clear();
    triggers::run(&state, &channels).await;
    {
        let msgs = sent.lock().unwrap();
        let recovered: Vec<_> = kinds(&msgs, "stale_data")
            .into_iter()
            .filter(|m| m.body.contains("flowing again"))
            .collect();
        assert_eq!(recovered.len(), 1, "one recovery notice");
    }
    assert_eq!(
        stale_state_count(&db).await,
        0,
        "state cleared after recovery"
    );
}

async fn insert_spot_reading(db: &DatabaseConnection, stream: &str, time_sql: &str) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index, measurement_type) \
             VALUES ('{stream}', '{site}', '{param}', {time_sql}, 50, 0, 'spot')",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    )
    .await;
}

/// Scenario: a slot whose grab series stops while its logger series keeps arriving.
/// Expected behaviour: the spot series alerts on its own observed interval and a healthy
/// continuous series next to it neither raises nor silences that alert.
#[tokio::test]
#[serial]
async fn spot_series_alerts_independently_of_continuous() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    crate::common::exec(&db, "DELETE FROM readings").await;
    let stream = turb_stream(&db).await;
    insert_spot_reading(&db, &stream, "NOW() - INTERVAL '40 days'").await;
    insert_spot_reading(&db, &stream, "NOW() - INTERVAL '39 days'").await;

    let sent = Arc::new(Mutex::new(Vec::new()));
    let channels: Vec<Box<dyn NotificationChannel>> =
        vec![Box::new(MockChannel { sent: sent.clone() })];
    triggers::run(&state, &channels).await;

    {
        let msgs = sent.lock().unwrap();
        let stale = kinds(&msgs, "stale_data");
        assert_eq!(stale.len(), 1, "the grab series is stale");
        assert!(
            stale[0].body.contains("grab samples"),
            "body: {}",
            stale[0].body
        );
    }

    insert_reading(&db, &stream, "NOW()").await;
    sent.lock().unwrap().clear();
    triggers::run(&state, &channels).await;
    assert!(
        kinds(&sent.lock().unwrap(), "stale_data").is_empty(),
        "a live continuous series neither resolves nor re-raises the spot alert"
    );
    assert_eq!(
        stale_state_count(&db).await,
        1,
        "the spot series stays firing on its own key"
    );
}

/// Scenario: every stream in a cycle fails to ingest. The driver still returns Ok, so the cycle is
/// recorded 'partial' rather than 'failed'.
/// Expected behaviour: the digest fires on partials too, then holds off for the suppression window.
#[tokio::test]
#[serial]
async fn sync_digest_covers_partial_cycles() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    crate::common::exec(
        &db,
        "INSERT INTO sync_services (id, service_type, instance_id, status, created_at, updated_at) \
         VALUES ('11111111-1111-1111-1111-111111111111', 'rshiny', 'metalp-1', 'registered', now(), now())",
    )
    .await;
    crate::common::exec(
        &db,
        "INSERT INTO sync_events (service_id, event_type, status, errors, started_at) \
         VALUES ('11111111-1111-1111-1111-111111111111', 'sync', 'partial', \
                 '[\"stream 7: reading rejected\"]'::jsonb, NOW() - INTERVAL '5 minutes')",
    )
    .await;

    let sent = Arc::new(Mutex::new(Vec::new()));
    let channels: Vec<Box<dyn NotificationChannel>> =
        vec![Box::new(MockChannel { sent: sent.clone() })];
    triggers::run(&state, &channels).await;

    {
        let msgs = sent.lock().unwrap();
        let digests = kinds(&msgs, "sync_failure");
        assert_eq!(digests.len(), 1, "partial cycles reach the digest");
        assert!(
            digests[0].body.contains("1 partial"),
            "body: {}",
            digests[0].body
        );
        assert!(
            digests[0].body.contains("reading rejected"),
            "the digest carries the error that caused it: {}",
            digests[0].body
        );
    }

    crate::common::exec(
        &db,
        "INSERT INTO sync_events (service_id, event_type, status, started_at) \
         VALUES ('11111111-1111-1111-1111-111111111111', 'sync', 'partial', NOW())",
    )
    .await;
    sent.lock().unwrap().clear();
    triggers::run(&state, &channels).await;
    assert!(
        kinds(&sent.lock().unwrap(), "sync_failure").is_empty(),
        "a repeating partial is suppressed within the window"
    );
}

/// Scenario: a sync registers a station nobody has paired, and a portal edit lands on a curated
/// reading. Both wait for a person on a screen nobody has a reason to open.
/// Expected behaviour: each raises its own notification, once, and the unpaired one stops once the
/// stream is paired.
#[tokio::test]
#[serial]
async fn unpaired_streams_and_open_holds_are_announced() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let stream = uuid::Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active, last_data_time) \
             VALUES ('{stream}', 'cnet', 'S99:DOC_avg_ppb', 'S99 DOC', true, NOW())"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO replicate_audit_holds \
                 (stream_id, site_id, parameter_id, group_time, kind, expected, computed, delta, status) \
             VALUES ('{stream}', NULL, NULL, NOW(), 'source_modified', '{{}}'::jsonb, \
                     '{{}}'::jsonb, '{{}}'::jsonb, 'deferred')"
        ),
    )
    .await;

    let sent = Arc::new(Mutex::new(Vec::new()));
    let channels: Vec<Box<dyn NotificationChannel>> =
        vec![Box::new(MockChannel { sent: sent.clone() })];
    triggers::run(&state, &channels).await;

    {
        let msgs = sent.lock().unwrap();
        let unpaired = kinds(&msgs, "streams_unpaired");
        assert_eq!(unpaired.len(), 1, "one digest for the one source system");
        assert!(unpaired[0].body.contains("S99 DOC"), "body: {}", unpaired[0].body);
        let holds = kinds(&msgs, "holds_open");
        assert_eq!(holds.len(), 1, "the review queue is announced");
        assert!(
            holds[0].body.contains("1 source_modified"),
            "the digest says what is waiting: {}",
            holds[0].body
        );
    }

    // Both conditions still stand, so neither repeats.
    sent.lock().unwrap().clear();
    triggers::run(&state, &channels).await;
    {
        let msgs = sent.lock().unwrap();
        assert!(kinds(&msgs, "streams_unpaired").is_empty(), "no re-announcement");
        assert!(kinds(&msgs, "holds_open").is_empty(), "within the suppression window");
    }

    // Pairing is the operator's own action: it clears the state silently, and an unpairing later
    // reads as a fresh discovery.
    crate::common::exec(
        &db,
        &format!(
            "UPDATE data_streams SET site_parameter_id = '{}', paired_at = NOW() WHERE id = '{stream}'",
            crate::common::PARAM_S1_TURB_ID
        ),
    )
    .await;
    sent.lock().unwrap().clear();
    triggers::run(&state, &channels).await;
    assert!(
        kinds(&sent.lock().unwrap(), "streams_unpaired").is_empty(),
        "a paired stream sends nothing"
    );

    crate::common::exec(
        &db,
        &format!("UPDATE data_streams SET site_parameter_id = NULL WHERE id = '{stream}'"),
    )
    .await;
    sent.lock().unwrap().clear();
    triggers::run(&state, &channels).await;
    assert_eq!(
        kinds(&sent.lock().unwrap(), "streams_unpaired").len(),
        1,
        "unpairing it again is a fresh discovery"
    );
}

/// Scenario: a plan apply spends its retries and ends failed, which is how B189's five rows sat on
/// the dev database unseen.
#[tokio::test]
#[serial]
async fn a_failed_job_is_announced_once_per_kind() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    crate::common::exec(
        &db,
        "INSERT INTO reprocessing_jobs (trigger_type, status, error_message, completed_at, detail) \
         VALUES ('plan_apply', 'failed', \
                 'duplicate key value violates unique constraint \"uq_site_param_name\"', \
                 NOW(), '{\"scope\": {\"plan_id\": \"p1\"}}'::jsonb)",
    )
    .await;
    crate::common::exec(
        &db,
        "INSERT INTO reprocessing_jobs (trigger_type, status, completed_at) \
         VALUES ('plan_apply', 'cancelled', NOW())",
    )
    .await;

    let sent = Arc::new(Mutex::new(Vec::new()));
    let channels: Vec<Box<dyn NotificationChannel>> =
        vec![Box::new(MockChannel { sent: sent.clone() })];
    triggers::run(&state, &channels).await;
    {
        let msgs = sent.lock().unwrap();
        let failed = kinds(&msgs, "job_failed");
        assert_eq!(failed.len(), 1, "one digest for the one failing kind");
        assert!(
            failed[0].body.contains("1 plan_apply job(s) failed"),
            "the cancelled job is not counted: {}",
            failed[0].body
        );
        assert!(
            failed[0].body.contains("uq_site_param_name"),
            "the reason travels with it: {}",
            failed[0].body
        );
        assert!(
            failed[0].body.contains("plan_id"),
            "the scope the run recorded travels with it: {}",
            failed[0].body
        );
    }

    // A second failure of the same kind inside the window is the same broken thing.
    crate::common::exec(
        &db,
        "INSERT INTO reprocessing_jobs (trigger_type, status, error_message, completed_at) \
         VALUES ('plan_apply', 'failed', 'and again', NOW())",
    )
    .await;
    sent.lock().unwrap().clear();
    triggers::run(&state, &channels).await;
    assert!(
        kinds(&sent.lock().unwrap(), "job_failed").is_empty(),
        "within the suppression window"
    );

    // Another kind failing is another thing broken, and says so on the same tick.
    crate::common::exec(
        &db,
        "INSERT INTO reprocessing_jobs (trigger_type, status, error_message, completed_at) \
         VALUES ('measurement_retag', 'failed', 'deadlock detected', NOW())",
    )
    .await;
    sent.lock().unwrap().clear();
    triggers::run(&state, &channels).await;
    {
        let msgs = sent.lock().unwrap();
        let failed = kinds(&msgs, "job_failed");
        assert_eq!(failed.len(), 1, "the new kind alone");
        assert!(failed[0].subject.contains("measurement_retag"), "{}", failed[0].subject);
    }
}
