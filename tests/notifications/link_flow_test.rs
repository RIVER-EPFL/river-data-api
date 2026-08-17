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
        format!(
            "SELECT telegram_chat_id FROM telegram_identities WHERE linked_keycloak_sub = '{sub}'"
        ),
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

    let (claimed, reply) = commands::start(&db, 555, Some(555), "abcd2345", None).await;
    assert!(claimed, "reply: {reply}");
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

    let (claimed, reply) = commands::start(&db, 777, Some(777), "expired99", None).await;
    assert!(!claimed, "an expired code must not link: {reply}");
    assert!(reply.contains("invalid or has expired"), "reply: {reply}");
    assert_eq!(
        chat_id_for(&db, "sub-bob").await,
        None,
        "expired code does not link"
    );
}

#[tokio::test]
#[serial]
async fn start_is_single_use() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    insert_pending(&db, "sub-carol", "single123", "1 hour").await;

    let (claimed, first) = commands::start(&db, 100, Some(100), "single123", None).await;
    assert!(claimed, "first: {first}");
    assert!(first.contains("Linked"), "first: {first}");

    // The same code can't link a second chat; it was cleared.
    let (claimed, second) = commands::start(&db, 200, Some(200), "single123", None).await;
    assert!(!claimed, "a reused code must not link: {second}");
    assert!(
        second.contains("invalid or has expired"),
        "second: {second}"
    );
    assert_eq!(
        chat_id_for(&db, "sub-carol").await,
        Some(100),
        "still the first chat"
    );
}

#[tokio::test]
#[serial]
async fn start_without_code_explains() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (claimed, reply) = commands::start(&db, 1, None, "", None).await;
    assert!(!claimed);
    assert!(reply.contains("link code"), "reply: {reply}");
}

/// Someone who finds the bot before the dashboard arrives with no code, so the reply has to carry
/// the whole route back rather than naming a code they have never seen.
#[tokio::test]
#[serial]
async fn a_dead_end_points_at_the_dashboard() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let (_, no_code) = commands::start(&db, 1, None, "", Some("https://river-data.epfl.ch/")).await;
    assert!(
        no_code.contains("https://river-data.epfl.ch/settings"),
        "reply: {no_code}"
    );

    let (_, bad_code) =
        commands::start(&db, 1, None, "nope", Some("https://river-data.epfl.ch")).await;
    assert!(
        bad_code.contains("https://river-data.epfl.ch/settings"),
        "reply: {bad_code}"
    );
}
