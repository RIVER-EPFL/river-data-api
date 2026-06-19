//! Per-user Telegram fan-out: an alert reaches only the linked chats whose subscriber has Telegram
//! enabled AND is subscribed to that slot. A chat with no subscriber row defaults to enabled +
//! subscribed (so chats linked before opting in still receive alerts). A site-level "off" override
//! suppresses that site only; a system-wide alert (no slot) ignores per-slot overrides.
//!
//! Exercises the recipient resolver directly so no real Telegram API call is made.
//!
//! Run: cargo test --test notifications -- --test-threads=1

use river_db::routes::private::notifications::{Slot, telegram::slot_recipients};
use sea_orm::DatabaseConnection;
use serial_test::serial;

async fn link_chat(db: &DatabaseConnection, sub: &str, chat_id: i64) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO telegram_identities (linked_keycloak_sub, telegram_chat_id, is_active) \
             VALUES ('{sub}', {chat_id}, TRUE)"
        ),
    )
    .await;
}

async fn subscriber(db: &DatabaseConnection, sub: &str, telegram_enabled: bool) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO notification_subscribers (keycloak_sub, telegram_enabled) \
             VALUES ('{sub}', {telegram_enabled})"
        ),
    )
    .await;
}

async fn mute_site(db: &DatabaseConnection, sub: &str) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO notification_subscriptions (keycloak_sub, site_id, enabled) \
             VALUES ('{sub}', '{site}', FALSE)",
            site = crate::common::SITE1_ID,
        ),
    )
    .await;
}

fn chat_ids(recips: &[(String, i64)]) -> Vec<i64> {
    let mut ids: Vec<i64> = recips.iter().map(|(_, c)| *c).collect();
    ids.sort_unstable();
    ids
}

#[tokio::test]
#[serial]
async fn telegram_fanout_respects_subscription_and_channel_toggle() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    // A: linked, no subscriber row → default enabled + subscribed.
    link_chat(&db, "sub-a", 111).await;
    // B: linked, telegram on, but muted this site.
    link_chat(&db, "sub-b", 222).await;
    subscriber(&db, "sub-b", true).await;
    mute_site(&db, "sub-b").await;
    // C: linked, telegram disabled.
    link_chat(&db, "sub-c", 333).await;
    subscriber(&db, "sub-c", false).await;

    let slot = Slot {
        project_id: None,
        site_id: crate::common::SITE1_ID.parse().unwrap(),
        parameter_id: crate::common::GLOBAL_PARAM_TURB_ID.parse().unwrap(),
    };

    // Slot-scoped: A only — B muted this site, C disabled Telegram.
    let scoped = slot_recipients(&db, &Some(slot)).await.unwrap();
    assert_eq!(chat_ids(&scoped), vec![111], "only the subscribed, telegram-enabled chat");

    // System-wide (no slot): B's per-site mute doesn't apply, so A and B; C still off.
    let all = slot_recipients(&db, &None).await.unwrap();
    assert_eq!(chat_ids(&all), vec![111, 222], "system-wide ignores per-slot overrides");
}

#[tokio::test]
#[serial]
async fn telegram_fanout_excludes_inactive_and_unlinked() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    link_chat(&db, "sub-active", 444).await;
    // Deactivated by the anti-backdoor sweep — must never receive alerts.
    crate::common::exec(
        &db,
        "INSERT INTO telegram_identities (linked_keycloak_sub, telegram_chat_id, is_active) \
         VALUES ('sub-revoked', 555, FALSE)",
    )
    .await;
    // Pending link (no chat_id yet) — not a deliverable recipient.
    crate::common::exec(
        &db,
        "INSERT INTO telegram_identities (linked_keycloak_sub, link_code, is_active) \
         VALUES ('sub-pending', 'abc23456', TRUE)",
    )
    .await;

    let all = slot_recipients(&db, &None).await.unwrap();
    assert_eq!(chat_ids(&all), vec![444], "only the active, claimed chat");
}
