//! The notification dispatcher drains the alarm_events outbox: open events with `notified_at IS
//! NULL` and resolved events with `resolution_notified_at IS NULL` are batched per kind and handed to
//! every channel. A mute stamps without sending; an all-channel failure leaves the row unstamped so
//! the next tick retries it.
//!
//! Alarm events are inserted directly so the dispatcher is exercised in isolation from the sweeper.
//!
//! Run: cargo test --test notifications -- --test-threads=1

use std::sync::{Arc, Mutex};

use river_db::common::AppState;
use river_db::routes::private::notifications::{
    DeliveryResult, NotificationChannel, OutgoingMessage, dispatcher,
};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

struct MockChannel {
    sent: Arc<Mutex<Vec<OutgoingMessage>>>,
    fail: bool,
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
            outcome: if self.fail {
                Err("boom".to_string())
            } else {
                Ok(())
            },
        }]
    }
}

fn channels(
    sent: &Arc<Mutex<Vec<OutgoingMessage>>>,
    fail: bool,
) -> Vec<Box<dyn NotificationChannel>> {
    vec![Box::new(MockChannel {
        sent: sent.clone(),
        fail,
    })]
}

async fn insert_open_event(db: &DatabaseConnection) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO alarm_events \
                (site_id, parameter_id, severity, max_severity, started_at, value_at_start, \
                 last_seen_at, last_value) \
             VALUES ('{site}', '{param}', 2, 2, NOW(), 600, NOW(), 600)",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    )
    .await;
}

async fn count(db: &DatabaseConnection, where_clause: &str) -> i64 {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT COUNT(*) AS c FROM alarm_events WHERE {where_clause}"),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "c")
    .unwrap()
}

async fn log_count(db: &DatabaseConnection, status: &str) -> i64 {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT COUNT(*) AS c FROM notification_log WHERE status = '{status}'"),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "c")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn opened_alarm_is_notified_and_stamped() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    insert_open_event(&db).await;
    assert_eq!(
        count(&db, "notified_at IS NULL AND resolved_at IS NULL").await,
        1
    );

    let sent = Arc::new(Mutex::new(Vec::new()));
    dispatcher::dispatch_once(&state, &channels(&sent, false)).await;

    let opened = sent
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m.kind == "alarm_opened")
        .count();
    assert_eq!(opened, 1, "one batched opened message");
    assert_eq!(
        count(&db, "notified_at IS NULL AND resolved_at IS NULL").await,
        0,
        "notified_at stamped after a successful send"
    );
    assert!(log_count(&db, "sent").await >= 1, "a sent row is logged");
}

#[tokio::test]
#[serial]
async fn muted_alarm_is_suppressed_but_stamped() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    insert_open_event(&db).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO notification_mutes (site_id, parameter_id, expires_at) \
             VALUES ('{site}', '{param}', NULL)",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    )
    .await;

    let sent = Arc::new(Mutex::new(Vec::new()));
    dispatcher::dispatch_once(&state, &channels(&sent, false)).await;

    assert!(
        sent.lock()
            .unwrap()
            .iter()
            .all(|m| m.kind != "alarm_opened"),
        "a muted slot sends no alarm notification"
    );
    assert_eq!(
        count(&db, "notified_at IS NULL AND resolved_at IS NULL").await,
        0,
        "muted event is still stamped so it isn't reconsidered"
    );
}

#[tokio::test]
#[serial]
async fn failed_delivery_is_not_stamped_and_retries() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    insert_open_event(&db).await;

    let sent = Arc::new(Mutex::new(Vec::new()));
    dispatcher::dispatch_once(&state, &channels(&sent, true)).await;

    let attempted = sent
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m.kind == "alarm_opened")
        .count();
    assert_eq!(attempted, 1, "the alarm delivery was attempted");
    assert_eq!(
        count(&db, "notified_at IS NULL AND resolved_at IS NULL").await,
        1,
        "an all-channel failure leaves the row unstamped for the next tick"
    );
    assert!(
        log_count(&db, "failed").await >= 1,
        "a failed row is logged"
    );
}

#[tokio::test]
#[serial]
async fn resolved_alarm_sends_resolution_notice() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    insert_open_event(&db).await;
    // Already announced the open; now resolve it.
    crate::common::exec(
        &db,
        "UPDATE alarm_events SET notified_at = NOW(), resolved_at = NOW(), resolved_value = 50, \
         resolution_notified_at = NULL",
    )
    .await;

    let sent = Arc::new(Mutex::new(Vec::new()));
    dispatcher::dispatch_once(&state, &channels(&sent, false)).await;

    let resolved = sent
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m.kind == "alarm_resolved")
        .count();
    assert_eq!(resolved, 1, "one batched resolved message");
    assert_eq!(
        count(
            &db,
            "resolved_at IS NOT NULL AND resolution_notified_at IS NULL"
        )
        .await,
        0,
        "resolution_notified_at stamped"
    );
}

#[tokio::test]
#[serial]
async fn concurrent_dispatchers_send_each_event_once() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    insert_open_event(&db).await;

    // Two replicas drain the same outbox at once; the FOR UPDATE SKIP LOCKED claim must let only one
    // send. A shared sink counts every send across both.
    let sent = Arc::new(Mutex::new(Vec::new()));
    let ch_a = channels(&sent, false);
    let ch_b = channels(&sent, false);
    tokio::join!(
        dispatcher::dispatch_once(&state, &ch_a),
        dispatcher::dispatch_once(&state, &ch_b),
    );

    let opened = sent
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m.kind == "alarm_opened")
        .count();
    assert_eq!(
        opened, 1,
        "exactly one replica sends the alarm, no duplicate alerts"
    );
    assert_eq!(
        count(&db, "notified_at IS NULL AND resolved_at IS NULL").await,
        0,
        "the event is stamped exactly once"
    );
}
