//! Scenario: the bot records what it receives, so a linked chat's use of it can be reviewed.
//!
//! Expected behaviour: every inbound message leaves exactly one row naming the command and how it
//! was resolved; no message body is stored; and a link claim names the identity it created.
//!
//! These drive `bot::route` directly, which is the seam a live update reaches after parsing.

use river_db::routes::private::notifications::{audit, bot};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;
const SUB: &str = "audit-user-sub";

async fn rows(db: &DatabaseConnection) -> Vec<(String, String, Option<String>)> {
    db.query_all(Statement::from_string(
        PG,
        "SELECT command, outcome, keycloak_sub FROM telegram_command_audit \
         ORDER BY created_at, command"
            .to_string(),
    ))
    .await
    .unwrap()
    .iter()
    .map(|r| {
        (
            r.try_get("", "command").unwrap(),
            r.try_get("", "outcome").unwrap(),
            r.try_get("", "keycloak_sub").unwrap(),
        )
    })
    .collect()
}

async fn link(db: &DatabaseConnection, chat_id: i64, active: bool) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO telegram_identities \
             (id, linked_keycloak_sub, telegram_chat_id, is_active, created_at, updated_at) \
             VALUES (gen_random_uuid(), '{SUB}', {chat_id}, {active}, NOW(), NOW())"
        ),
    )
    .await;
}

#[tokio::test]
#[serial]
async fn an_unlinked_chat_is_recorded() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    bot::route(
        &state,
        &bot::Limits::new(),
        &bot::Inbound::private(424_242, None),
        "status",
        "",
    )
    .await;

    let recorded = rows(&db).await;
    assert_eq!(recorded.len(), 1, "one message, one row: {recorded:?}");
    assert_eq!(recorded[0].0, "status");
    assert_eq!(recorded[0].1, "unlinked");
    assert_eq!(recorded[0].2, None, "an unlinked chat has no user to name");
}

#[tokio::test]
#[serial]
async fn a_deactivated_link_is_recorded_as_inactive() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    link(&db, 424_243, false).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    bot::route(
        &state,
        &bot::Limits::new(),
        &bot::Inbound::private(424_243, None),
        "alarms",
        "",
    )
    .await;

    let recorded = rows(&db).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        (recorded[0].0.as_str(), recorded[0].1.as_str()),
        ("alarms", "inactive")
    );
}

/// No Keycloak in this suite, so an active link cannot be resolved. That is the fail-closed path,
/// and it must be as visible in the trail as any other refusal.
#[tokio::test]
#[serial]
async fn an_unresolvable_user_is_recorded_as_unavailable() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    link(&db, 424_244, true).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    bot::route(
        &state,
        &bot::Limits::new(),
        &bot::Inbound::private(424_244, None),
        "latest",
        "",
    )
    .await;

    let recorded = rows(&db).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].1, "unavailable");
    assert_eq!(
        recorded[0].2.as_deref(),
        None,
        "a refusal before resolution names no user"
    );
}

/// Claiming a code is the moment a chat gains an identity, so the row must name it.
#[tokio::test]
#[serial]
async fn a_link_claim_is_recorded_against_the_user_it_created() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO telegram_identities \
             (id, linked_keycloak_sub, link_code, link_code_expires_at, is_active, \
              created_at, updated_at) \
             VALUES (gen_random_uuid(), '{SUB}', 'goodcode', NOW() + INTERVAL '10 minutes', \
                     TRUE, NOW(), NOW())"
        ),
    )
    .await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    bot::route(
        &state,
        &bot::Limits::new(),
        &bot::Inbound::private(424_245, Some(9_001)),
        "start",
        "goodcode",
    )
    .await;

    let recorded = rows(&db).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        (recorded[0].0.as_str(), recorded[0].1.as_str()),
        ("start", "ok")
    );
    assert_eq!(recorded[0].2.as_deref(), Some(SUB));
}

/// The security fix: a link code claimed in a group would hand every member of that group the
/// claiming user's project access.
#[tokio::test]
#[serial]
async fn a_link_code_cannot_be_claimed_in_a_group() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO telegram_identities \
             (id, linked_keycloak_sub, link_code, link_code_expires_at, is_active, \
              created_at, updated_at) \
             VALUES (gen_random_uuid(), '{SUB}', 'groupcode', NOW() + INTERVAL '10 minutes', \
                     TRUE, NOW(), NOW())"
        ),
    )
    .await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let group = bot::Inbound {
        chat_id: -100_500,
        chat_type: Some("supergroup".to_string()),
        is_private: false,
        from_id: Some(9_002),
    };
    let reply = bot::route(&state, &bot::Limits::new(), &group, "start", "groupcode")
        .await
        .expect("a refusal");
    assert!(
        reply.text().contains("1:1"),
        "the refusal must point at a direct chat: {}",
        reply.text()
    );
    assert!(
        !reply.text().contains("groupcode"),
        "and must never echo the code back into the group"
    );

    // The code was on screen for everyone in the group, so it must be burned rather than left live.
    let pending: i64 = db
        .query_one(Statement::from_string(
            PG,
            format!(
                "SELECT COUNT(*) AS n FROM telegram_identities \
                 WHERE linked_keycloak_sub = '{SUB}' AND link_code IS NOT NULL"
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "n")
        .unwrap();
    assert_eq!(pending, 0, "the exposed code must be cancelled");
    assert!(
        reply.text().contains("cancelled"),
        "and the group must be told: {}",
        reply.text()
    );

    let recorded = rows(&db).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        (recorded[0].0.as_str(), recorded[0].1.as_str()),
        ("start", "denied")
    );
}

/// Data minimisation: whatever someone types, only our own vocabulary is stored.
#[tokio::test]
#[serial]
async fn a_message_body_is_never_stored() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    bot::route(
        &state,
        &bot::Limits::new(),
        &bot::Inbound::private(424_246, None),
        "sekritcommand",
        "my private note about someone",
    )
    .await;

    let recorded = rows(&db).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].0, "unknown",
        "unknown input is not stored verbatim"
    );

    let dump: String = db
        .query_one(Statement::from_string(
            PG,
            "SELECT COALESCE(string_agg(command || ' ' || COALESCE(detail, ''), ' '), '') AS d \
             FROM telegram_command_audit"
                .to_string(),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "d")
        .unwrap();
    assert!(
        !dump.contains("sekrit") && !dump.contains("private note"),
        "no part of the message may reach the table: {dump}"
    );
}

#[tokio::test]
#[serial]
async fn retention_drops_only_rows_past_the_window() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    crate::common::exec(
        &db,
        "INSERT INTO telegram_command_audit (chat_id, command, outcome, created_at) VALUES \
         (1, 'status', 'ok', NOW() - INTERVAL '400 days'), \
         (2, 'status', 'ok', NOW() - INTERVAL '10 days')",
    )
    .await;

    let pruned = audit::prune(&db, 365).await.expect("prune");
    assert_eq!(pruned, 1);
    assert_eq!(rows(&db).await.len(), 1, "the recent row survives");

    assert_eq!(
        audit::prune(&db, 0).await.expect("prune"),
        0,
        "zero retention keeps everything"
    );
    assert_eq!(rows(&db).await.len(), 1);
}

/// The link belongs to the Telegram account that claimed it, not to whoever is in the chat.
#[tokio::test]
#[serial]
async fn a_message_from_another_account_is_refused() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO telegram_identities \
             (id, linked_keycloak_sub, telegram_chat_id, telegram_user_id, is_active, \
              created_at, updated_at) \
             VALUES (gen_random_uuid(), '{SUB}', 424247, 5000, TRUE, NOW(), NOW())"
        ),
    )
    .await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let reply = bot::route(
        &state,
        &bot::Limits::new(),
        &bot::Inbound::private(424_247, Some(6000)),
        "status",
        "",
    )
    .await
    .expect("a refusal");
    assert!(
        reply.text().contains("different Telegram account"),
        "{}",
        reply.text()
    );

    let recorded = rows(&db).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].1, "wrong_account");
}

/// A link claimed before the account was recorded adopts the first sender it sees, so an existing
/// link keeps working and gains the binding.
#[tokio::test]
#[serial]
async fn an_unbound_link_adopts_its_sender() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    link(&db, 424_248, true).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    bot::route(
        &state,
        &bot::Limits::new(),
        &bot::Inbound::private(424_248, Some(7000)),
        "status",
        "",
    )
    .await;

    let bound: Option<i64> = db
        .query_one(Statement::from_string(
            PG,
            "SELECT telegram_user_id FROM telegram_identities WHERE telegram_chat_id = 424248"
                .to_string(),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "telegram_user_id")
        .unwrap();
    assert_eq!(bound, Some(7000));
}
