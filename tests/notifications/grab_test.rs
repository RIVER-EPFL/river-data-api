//! `/grab` submits a grab sample from the field via the bot, reusing the full grab-sample insert
//! path. Site and parameter are matched case-insensitively by name.
//!
//! Run: cargo test --test notifications -- --test-threads=1

use river_db::common::authz::AccessScope;
use river_db::routes::private::notifications::commands;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

async fn spot_count(db: &DatabaseConnection, raw_value: f64) -> i64 {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT COUNT(*) AS c FROM readings \
             WHERE site_id = '{site}' AND parameter_id = '{param}' \
               AND measurement_type = 'spot' AND raw_value = {raw_value}",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "c")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn grab_records_a_sample() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let reply = commands::grab(&state, &AccessScope::Unrestricted, "upstream turbidity 12.3", Some("alice"), 99).await;
    assert!(reply.contains("Recorded 1"), "reply: {reply}");
    assert_eq!(spot_count(&db, 12.3).await, 1, "a spot reading was written");
}

#[tokio::test]
#[serial]
async fn grab_records_replicates() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let reply = commands::grab(&state, &AccessScope::Unrestricted, "upstream turbidity 10 11 12", Some("alice"), 99).await;
    assert!(reply.contains("Recorded 3"), "reply: {reply}");
    assert_eq!(spot_count(&db, 10.0).await, 1);
    assert_eq!(spot_count(&db, 11.0).await, 1);
    assert_eq!(spot_count(&db, 12.0).await, 1);
}

#[tokio::test]
#[serial]
async fn grab_rejects_unknown_site() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let reply = commands::grab(&state, &AccessScope::Unrestricted, "nosuchplace turbidity 5", Some("alice"), 99).await;
    assert!(reply.contains("No site matches"), "reply: {reply}");
}

#[tokio::test]
#[serial]
async fn grab_rejects_non_numeric_value() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let reply = commands::grab(&state, &AccessScope::Unrestricted, "upstream turbidity oops", Some("alice"), 99).await;
    assert!(reply.contains("not a number"), "reply: {reply}");
}
