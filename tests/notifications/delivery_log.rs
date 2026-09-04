//! The delivery log read back by message. Every `deliver` call writes one row per (channel,
//! recipient) sharing an alarm event, a kind and the second they were written in, so the read path
//! has to regroup them into the message the admin is asking about.
//!
//! Run: cargo test --test notifications delivery_log -- --test-threads=1

use river_db::routes::private::notifications::deliveries::{DeliveryQuery, list_deliveries};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

fn query(
    status: Option<&str>,
    kind: Option<&str>,
    limit: Option<u64>,
    offset: Option<u64>,
) -> DeliveryQuery {
    DeliveryQuery {
        limit,
        offset,
        status: status.map(str::to_string),
        kind: kind.map(str::to_string),
    }
}

async fn open_event(db: &DatabaseConnection) -> String {
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
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        "SELECT id::text AS id FROM alarm_events ORDER BY started_at DESC LIMIT 1".to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<String>("", "id")
    .unwrap()
}

async fn log_row(
    db: &DatabaseConnection,
    event: Option<&str>,
    kind: &str,
    channel: &str,
    recipient: &str,
    status: &str,
    error: Option<&str>,
    at: &str,
) {
    let event = event.map_or("NULL".to_string(), |e| format!("'{e}'"));
    let error = error.map_or("NULL".to_string(), |e| format!("'{e}'"));
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO notification_log \
                (alarm_event_id, kind, channel, recipient, status, error, created_at) \
             VALUES ({event}, '{kind}', '{channel}', '{recipient}', '{status}', {error}, '{at}')"
        ),
    )
    .await;
}

async fn seeded() -> (DatabaseConnection, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let event = open_event(&db).await;

    // One alarm delivered to two devices, one of which is dead.
    log_row(
        &db,
        Some(&event),
        "alarm_opened",
        "web_push",
        "device-a",
        "sent",
        None,
        "2026-09-01T10:00:00Z",
    )
    .await;
    log_row(
        &db,
        Some(&event),
        "alarm_opened",
        "web_push",
        "device-b",
        "failed",
        Some("410 gone"),
        "2026-09-01T10:00:00Z",
    )
    .await;
    // A later message on a muted slot, which reaches nobody by design.
    log_row(
        &db,
        None,
        "sync_stale",
        "all",
        "slot:x:y",
        "muted",
        None,
        "2026-09-01T11:00:00Z",
    )
    .await;
    (db, event)
}

#[tokio::test]
#[serial]
async fn rows_of_one_delivery_are_read_back_as_one_message_with_its_recipients() {
    let (db, event) = seeded().await;

    let page = list_deliveries(&db, &query(None, None, None, None))
        .await
        .expect("delivery log reads");

    assert_eq!(page.total, 2, "two messages, not three rows");
    assert_eq!(page.messages.len(), 2);

    let newest = &page.messages[0];
    assert_eq!(newest.kind, "sync_stale", "newest message first");
    assert_eq!(newest.counts.muted, 1);
    assert_eq!(newest.counts.total, 1);
    assert!(newest.alarm_event_id.is_none());

    let alarm = &page.messages[1];
    assert_eq!(alarm.alarm_event_id.map(|id| id.to_string()), Some(event));
    assert_eq!(alarm.counts.total, 2);
    assert_eq!(alarm.counts.sent, 1);
    assert_eq!(alarm.counts.failed, 1);
    assert_eq!(alarm.site_name.as_deref(), Some("Upstream Station"));
    let mut recipients: Vec<&str> = alarm
        .recipients
        .iter()
        .map(|r| r.recipient.as_str())
        .collect();
    recipients.sort_unstable();
    assert_eq!(
        recipients,
        vec!["device-a", "device-b"],
        "both recipients listed"
    );
    let dead = alarm
        .recipients
        .iter()
        .find(|r| r.recipient == "device-b")
        .expect("the failed recipient is listed");
    assert_eq!(dead.status, "failed");
    assert_eq!(dead.error.as_deref(), Some("410 gone"));
}

#[tokio::test]
#[serial]
async fn a_status_filter_keeps_the_messages_carrying_that_status_whole() {
    let (db, _) = seeded().await;

    let page = list_deliveries(&db, &query(Some("failed"), None, None, None))
        .await
        .expect("filtered log reads");
    assert_eq!(page.total, 1, "only the alarm message carries a failure");
    assert_eq!(page.messages.len(), 1);
    // The message is kept whole: the delivery that succeeded is still shown beside the one that
    // did not, which is the comparison the panel exists for.
    assert_eq!(page.messages[0].recipients.len(), 2);
    assert_eq!(page.messages[0].counts.sent, 1);

    let none = list_deliveries(&db, &query(Some("skipped"), None, None, None))
        .await
        .expect("a status nothing carries is empty, not an error");
    assert_eq!(none.total, 0);
    assert!(none.messages.is_empty());

    let bad = list_deliveries(&db, &query(Some("delivered"), None, None, None)).await;
    assert!(
        bad.is_err(),
        "an unknown status is refused, not silently empty"
    );
}

#[tokio::test]
#[serial]
async fn the_page_is_counted_in_messages_and_the_kind_filter_narrows_it() {
    let (db, _) = seeded().await;

    let first = list_deliveries(&db, &query(None, None, Some(1), None))
        .await
        .expect("first page reads");
    assert_eq!(first.messages.len(), 1);
    assert_eq!(first.total, 2, "total counts messages the filter matched");
    assert_eq!(first.messages[0].kind, "sync_stale");

    let second = list_deliveries(&db, &query(None, None, Some(1), Some(1)))
        .await
        .expect("second page reads");
    assert_eq!(second.messages.len(), 1);
    assert_eq!(second.messages[0].kind, "alarm_opened");
    assert_eq!(
        second.messages[0].recipients.len(),
        2,
        "the offset page carries its own rows"
    );

    let by_kind = list_deliveries(&db, &query(None, Some("sync_stale"), None, None))
        .await
        .expect("kind filter reads");
    assert_eq!(by_kind.total, 1);
    assert_eq!(by_kind.messages[0].kind, "sync_stale");
}

#[tokio::test]
#[serial]
async fn an_empty_log_is_an_empty_page() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let page = list_deliveries(&db, &query(None, None, None, None))
        .await
        .expect("an empty log reads");
    assert_eq!(page.total, 0);
    assert!(page.messages.is_empty());
}
