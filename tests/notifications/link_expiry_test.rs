//! Scenario: a Telegram link that nobody uses should lapse, so a departed collaborator's chat does
//! not keep receiving site data.
//!
//! Expected behaviour: "used" means any activity, sent *or* received; the holder is warned before
//! expiry and told when it happens; an admin can pin a link against inactivity but never against
//! revocation.
//!
//! These drive `reconcile::sweep` directly. No Telegram token is configured in tests, so the
//! outbound messages are skipped, which is exactly what makes the "the warning must not stamp
//! activity" assertion meaningful: the sweep must not touch `last_verified_at` on its own.

use river_db::routes::private::notifications::reconcile;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;

const SUB: &str = "expiry-user-sub";

async fn insert_identity(db: &DatabaseConnection, chat_id: i64, idle_days: i64, exempt: bool) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO telegram_identities \
             (id, linked_keycloak_sub, telegram_chat_id, is_active, expiry_exempt, \
              last_verified_at, created_at, updated_at) \
             VALUES (gen_random_uuid(), '{SUB}', {chat_id}, TRUE, {exempt}, \
                     NOW() - INTERVAL '{idle_days} days', NOW(), NOW())"
        ),
    )
    .await;
}

async fn row(db: &DatabaseConnection, chat_id: i64) -> (bool, Option<chrono::DateTime<chrono::Utc>>) {
    let r = db
        .query_one(Statement::from_string(
            PG,
            format!(
                "SELECT is_active, last_verified_at FROM telegram_identities \
                 WHERE telegram_chat_id = {chat_id}"
            ),
        ))
        .await
        .unwrap()
        .expect("identity row");
    (
        r.try_get("", "is_active").unwrap(),
        r.try_get("", "last_verified_at").ok().flatten(),
    )
}

async fn count(db: &DatabaseConnection, chat_id: i64) -> i64 {
    db.query_one(Statement::from_string(
        PG,
        format!("SELECT COUNT(*) AS n FROM telegram_identities WHERE telegram_chat_id = {chat_id}"),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn an_idle_link_is_deactivated() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    insert_identity(&db, 900_001, 45, false).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let outcome = reconcile::sweep(&state).await.expect("sweep");
    assert_eq!(outcome.expired, 1, "a 45-day idle link lapses at 30 days");
    assert!(!row(&db, 900_001).await.0, "the link must be deactivated");
}

#[tokio::test]
#[serial]
async fn a_recently_used_link_survives() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    insert_identity(&db, 900_002, 3, false).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let outcome = reconcile::sweep(&state).await.expect("sweep");
    assert_eq!(outcome.expired, 0);
    assert_eq!(outcome.warned, 0, "three days idle is not close to expiry");
    assert!(row(&db, 900_002).await.0, "the link must stay active");
}

/// The regression test for the easiest thing to get wrong here: if the warning went out through
/// `TelegramChannel::deliver` it would stamp `last_verified_at`, reset the clock, and the link
/// would never lapse.
#[tokio::test]
#[serial]
async fn the_expiry_sweep_does_not_stamp_activity() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    insert_identity(&db, 900_003, 25, false).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let before = row(&db, 900_003).await.1.expect("stamped");
    reconcile::sweep(&state).await.expect("sweep");
    let after = row(&db, 900_003).await.1.expect("stamped");

    assert_eq!(
        before, after,
        "warning about expiry must not reset the idle clock"
    );
}

#[tokio::test]
#[serial]
async fn a_pinned_link_never_lapses() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    insert_identity(&db, 900_004, 500, true).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let outcome = reconcile::sweep(&state).await.expect("sweep");
    assert_eq!(outcome.warned, 0);
    assert_eq!(outcome.expired, 0);
    assert_eq!(outcome.purged, 0);
    assert!(
        row(&db, 900_004).await.0,
        "a pinned link is exempt at every stage, even 500 days idle"
    );
    assert_eq!(count(&db, 900_004).await, 1, "and is never purged");
}

#[tokio::test]
#[serial]
async fn a_long_dead_link_is_purged() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO telegram_identities \
             (id, linked_keycloak_sub, telegram_chat_id, is_active, expiry_exempt, \
              last_verified_at, created_at, updated_at) \
             VALUES (gen_random_uuid(), '{SUB}', 900005, FALSE, FALSE, \
                     NOW() - INTERVAL '200 days', NOW(), NOW())"
        ),
    )
    .await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let outcome = reconcile::sweep(&state).await.expect("sweep");
    assert_eq!(outcome.purged, 1);
    assert_eq!(count(&db, 900_005).await, 0, "the row is gone");
}

#[tokio::test]
#[serial]
async fn a_deactivated_link_inside_the_grace_period_is_kept() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO telegram_identities \
             (id, linked_keycloak_sub, telegram_chat_id, is_active, expiry_exempt, \
              last_verified_at, created_at, updated_at) \
             VALUES (gen_random_uuid(), '{SUB}', 900006, FALSE, FALSE, \
                     NOW() - INTERVAL '40 days', NOW(), NOW())"
        ),
    )
    .await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let outcome = reconcile::sweep(&state).await.expect("sweep");
    assert_eq!(outcome.purged, 0, "40 days is inside the 90-day grace period");
    assert_eq!(count(&db, 900_006).await, 1);
}

/// A link with no recorded activity has nothing to measure idleness against, so it must be left
/// alone rather than treated as infinitely idle.
#[tokio::test]
#[serial]
async fn a_link_with_no_activity_stamp_is_left_alone() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO telegram_identities \
             (id, linked_keycloak_sub, telegram_chat_id, is_active, expiry_exempt, \
              last_verified_at, created_at, updated_at) \
             VALUES (gen_random_uuid(), '{SUB}', 900007, TRUE, FALSE, NULL, \
                     NOW() - INTERVAL '400 days', NOW())"
        ),
    )
    .await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let outcome = reconcile::sweep(&state).await.expect("sweep");
    assert_eq!(outcome.expired, 0);
    assert!(row(&db, 900_007).await.0);
}
