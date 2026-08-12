//! A mute is a property of a `(site, parameter)` slot, so it suppresses every slot-keyed
//! notification, not only the threshold alarms. The gate sits on the single delivery path, and
//! these cover its edges: an expired mute, a mute on a neighbouring slot, and a message that
//! carries no slot at all.
//!
//! Run: cargo test --test notifications -- --test-threads=1

use std::sync::{Arc, Mutex};

use river_db::common::AppState;
use river_db::routes::private::notifications::{
    DeliveryResult, NotificationChannel, OutgoingMessage, dispatcher, triggers,
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

fn channels(sent: &Arc<Mutex<Vec<OutgoingMessage>>>) -> Vec<Box<dyn NotificationChannel>> {
    vec![Box::new(MockChannel { sent: sent.clone() })]
}

async fn scalar_count(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "c")
    .unwrap()
}

async fn mute(db: &DatabaseConnection, parameter_id: &str, expires_sql: &str) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO notification_mutes (site_id, parameter_id, expires_at) \
             VALUES ('{site}', '{parameter_id}', {expires_sql})",
            site = crate::common::SITE1_ID,
        ),
    )
    .await;
}

async fn stream_of(db: &DatabaseConnection, site_parameter_id: &str) -> String {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT id FROM data_streams WHERE site_parameter_id = '{site_parameter_id}'"),
    ))
    .await
    .unwrap()
    .expect("seeded stream")
    .try_get::<uuid::Uuid>("", "id")
    .unwrap()
    .to_string()
}

/// One stale reading on the turbidity slot and nothing else, so exactly one slot is stale.
async fn one_stale_slot(db: &DatabaseConnection) {
    crate::common::exec(db, "DELETE FROM readings").await;
    let stream = stream_of(db, crate::common::PARAM_S1_TURB_ID).await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{stream}', '{site}', '{param}', NOW() - INTERVAL '10 hours', 50, 0)",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    )
    .await;
}

fn of_kind<'a>(msgs: &'a [OutgoingMessage], kind: &str) -> Vec<&'a OutgoingMessage> {
    msgs.iter().filter(|m| m.kind == kind).collect()
}

#[tokio::test]
#[serial]
async fn a_muted_slot_raises_no_stale_data_alert() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    one_stale_slot(&db).await;
    mute(&db, crate::common::GLOBAL_PARAM_TURB_ID, "NULL").await;

    let sent = Arc::new(Mutex::new(Vec::new()));
    triggers::run(&state, &channels(&sent)).await;

    assert!(
        of_kind(&sent.lock().unwrap(), "stale_data").is_empty(),
        "a permanent mute suppresses the stale-data alert"
    );
    assert_eq!(
        scalar_count(
            &db,
            "SELECT COUNT(*) AS c FROM notification_log WHERE kind = 'stale_data'"
        )
        .await,
        0,
        "a suppressed alert is not logged as a delivery"
    );
    assert_eq!(
        scalar_count(
            &db,
            "SELECT COUNT(*) AS c FROM notification_state WHERE kind = 'stale_data'"
        )
        .await,
        1,
        "the condition is still tracked, so unmuting does not replay the backlog"
    );
}

#[tokio::test]
#[serial]
async fn an_expired_mute_does_not_suppress() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    one_stale_slot(&db).await;
    mute(
        &db,
        crate::common::GLOBAL_PARAM_TURB_ID,
        "NOW() - INTERVAL '1 second'",
    )
    .await;

    let sent = Arc::new(Mutex::new(Vec::new()));
    triggers::run(&state, &channels(&sent)).await;

    assert_eq!(
        of_kind(&sent.lock().unwrap(), "stale_data").len(),
        1,
        "a mute that has already expired is no longer in force"
    );
}

#[tokio::test]
#[serial]
async fn a_mute_on_a_neighbouring_slot_does_not_suppress() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    one_stale_slot(&db).await;
    mute(&db, crate::common::GLOBAL_PARAM_TEMP_ID, "NULL").await;

    let sent = Arc::new(Mutex::new(Vec::new()));
    triggers::run(&state, &channels(&sent)).await;

    let stale = of_kind(&sent.lock().unwrap(), "stale_data").len();
    assert_eq!(
        stale, 1,
        "muting the temperature slot leaves the turbidity slot audible"
    );
}

#[tokio::test]
#[serial]
async fn a_message_without_a_slot_is_never_muted() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    crate::common::exec(&db, "DELETE FROM readings").await;
    for parameter in [
        crate::common::GLOBAL_PARAM_TURB_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        crate::common::GLOBAL_PARAM_DO_ID,
        crate::common::GLOBAL_PARAM_COND_ID,
        crate::common::GLOBAL_PARAM_DEPTH_ID,
    ] {
        mute(&db, parameter, "NULL").await;
    }

    let (_raw, service_id) = crate::common::seed_sync_session_token(&db).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sync_events (id, service_id, event_type, status, readings_synced, \
                 status_events_synced, started_at) \
             VALUES ('{}', '{service_id}', 'scheduled', 'failed', 0, 0, NOW())",
            uuid::Uuid::new_v4()
        ),
    )
    .await;

    let sent = Arc::new(Mutex::new(Vec::new()));
    triggers::run(&state, &channels(&sent)).await;

    assert_eq!(
        of_kind(&sent.lock().unwrap(), "sync_failure").len(),
        1,
        "a sync-failure digest has no slot to mute against and still goes out"
    );
}

#[tokio::test]
#[serial]
async fn a_mute_added_after_the_open_alert_suppresses_the_resolution_notice() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO alarm_events \
                (site_id, parameter_id, severity, max_severity, started_at, value_at_start, \
                 last_seen_at, last_value, notified_at, resolved_at, resolved_value) \
             VALUES ('{site}', '{param}', 2, 2, NOW(), 600, NOW(), 600, NOW(), NOW(), 50)",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    )
    .await;
    mute(&db, crate::common::GLOBAL_PARAM_TURB_ID, "NULL").await;

    let sent = Arc::new(Mutex::new(Vec::new()));
    dispatcher::dispatch_once(&state, &channels(&sent)).await;

    assert!(
        of_kind(&sent.lock().unwrap(), "alarm_resolved").is_empty(),
        "the resolution notice for a muted slot is suppressed"
    );
    assert_eq!(
        scalar_count(
            &db,
            "SELECT COUNT(*) AS c FROM alarm_events WHERE resolution_notified_at IS NULL"
        )
        .await,
        0,
        "the suppressed event is stamped so it is not reconsidered every tick"
    );
}
