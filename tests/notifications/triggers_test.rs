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
    db.query_one(Statement::from_string(
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
    db.query_one(Statement::from_string(
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
    let channels: Vec<Box<dyn NotificationChannel>> = vec![Box::new(MockChannel { sent: sent.clone() })];
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
    assert!(kinds(&sent.lock().unwrap(), "stale_data").is_empty(), "no re-notify while still stale");

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
    assert_eq!(stale_state_count(&db).await, 0, "state cleared after recovery");
}
