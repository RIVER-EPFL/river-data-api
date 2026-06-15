//! The /start link claim: a one-time code binds a chat to a pending identity, exactly once, and
//! only while unexpired.
//!
//! Run: cargo test --test notifications -- --test-threads=1

use river_db::routes::private::notifications::commands;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

async fn insert_pending(db: &DatabaseConnection, sub: &str, code: &str, ttl: &str) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO telegram_identities \
                (linked_keycloak_sub, link_code, link_code_expires_at, is_active) \
             VALUES ('{sub}', '{code}', NOW() + INTERVAL '{ttl}', TRUE)"
        ),
    )
    .await;
}

async fn chat_id_for(db: &DatabaseConnection, sub: &str) -> Option<i64> {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT telegram_chat_id FROM telegram_identities WHERE linked_keycloak_sub = '{sub}'"),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<Option<i64>>("", "telegram_chat_id")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn start_claims_a_valid_code() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    insert_pending(&db, "sub-alice", "abcd2345", "1 hour").await;

    let reply = commands::start(&db, 555, Some("alice"), "abcd2345").await;
    assert!(reply.contains("Linked"), "reply: {reply}");
    assert_eq!(chat_id_for(&db, "sub-alice").await, Some(555));

    // Code is cleared on claim.
    let code: Option<String> = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT link_code FROM telegram_identities WHERE linked_keycloak_sub = 'sub-alice'"
                .to_string(),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<Option<String>>("", "link_code")
        .unwrap();
    assert_eq!(code, None, "link_code cleared after claim");
}

#[tokio::test]
#[serial]
async fn start_rejects_expired_code() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    insert_pending(&db, "sub-bob", "expired99", "-1 hour").await;

    let reply = commands::start(&db, 777, Some("bob"), "expired99").await;
    assert!(reply.contains("Invalid or expired"), "reply: {reply}");
    assert_eq!(chat_id_for(&db, "sub-bob").await, None, "expired code does not link");
}

#[tokio::test]
#[serial]
async fn start_is_single_use() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    insert_pending(&db, "sub-carol", "single123", "1 hour").await;

    let first = commands::start(&db, 100, Some("carol"), "single123").await;
    assert!(first.contains("Linked"), "first: {first}");

    // The same code can't link a second chat — it was cleared.
    let second = commands::start(&db, 200, Some("mallory"), "single123").await;
    assert!(second.contains("Invalid or expired"), "second: {second}");
    assert_eq!(chat_id_for(&db, "sub-carol").await, Some(100), "still the first chat");
}

#[tokio::test]
#[serial]
async fn start_without_code_explains() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let reply = commands::start(&db, 1, None, "").await;
    assert!(reply.contains("link code"), "reply: {reply}");
}
