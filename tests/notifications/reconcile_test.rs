//! `reconcile::sweep` prunes push subscriptions and deactivates subscribers whose Keycloak account
//! resolves as `Revoked`. An active member is left alone, and an unresolvable account (`None`, e.g.
//! Keycloak unreachable) is retained rather than swept, so a transient outage cannot deactivate a
//! live user.
//!
//! Run: cargo test --test notifications reconcile -- --test-threads=1

use river_db::common::authz::Role;
use river_db::routes::private::notifications::access::RoleResolution;
use river_db::routes::private::notifications::reconcile;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

async fn scalar_count(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "c")
    .unwrap()
}

async fn is_active(db: &DatabaseConnection, sub: &str) -> bool {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT is_active FROM notification_subscribers WHERE keycloak_sub = '{sub}'"),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<bool>("", "is_active")
    .unwrap()
}

async fn seed_rows(db: &DatabaseConnection, sub: &str) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO web_push_subscriptions (keycloak_sub, endpoint, p256dh, auth) \
             VALUES ('{sub}', 'https://push.example/{sub}', 'p256dh', 'auth')"
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO notification_subscribers (keycloak_sub, is_active) \
             VALUES ('{sub}', TRUE)"
        ),
    )
    .await;
}

#[tokio::test]
#[serial]
async fn sweep_prunes_revoked_keeps_active_and_unresolvable() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    seed_rows(&db, "sub-revoked").await;
    seed_rows(&db, "sub-active").await;
    seed_rows(&db, "sub-none").await;

    state
        .authorizer
        .prime("sub-revoked", RoleResolution::Revoked)
        .await;
    state
        .authorizer
        .prime("sub-active", RoleResolution::Active(Role::River))
        .await;
    // sub-none is left unprimed: with no Keycloak backend `resolve` returns None (fail open on an
    // unreachable authority), so the sweep must retain it.

    let outcome = reconcile::sweep(&state).await.unwrap();

    assert_eq!(outcome.revoked, 1, "one push subscription pruned");
    assert_eq!(outcome.deactivated, 1, "one subscriber deactivated");

    assert_eq!(
        scalar_count(
            &db,
            "SELECT COUNT(*) AS c FROM web_push_subscriptions WHERE keycloak_sub = 'sub-revoked'"
        )
        .await,
        0,
        "revoked user's push subscription is deleted"
    );
    assert_eq!(
        scalar_count(
            &db,
            "SELECT COUNT(*) AS c FROM web_push_subscriptions WHERE keycloak_sub = 'sub-active'"
        )
        .await,
        1,
        "active member's push subscription is retained"
    );
    assert_eq!(
        scalar_count(
            &db,
            "SELECT COUNT(*) AS c FROM web_push_subscriptions WHERE keycloak_sub = 'sub-none'"
        )
        .await,
        1,
        "unresolvable user's push subscription is retained"
    );

    assert!(!is_active(&db, "sub-revoked").await, "revoked subscriber deactivated");
    assert!(is_active(&db, "sub-active").await, "active subscriber untouched");
    assert!(is_active(&db, "sub-none").await, "unresolvable subscriber untouched");

    crate::common::cleanup_test_db(&db).await;
}
