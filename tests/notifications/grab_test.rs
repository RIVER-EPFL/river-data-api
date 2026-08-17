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

    let reply = commands::grab(
        &state,
        &AccessScope::Unrestricted,
        "upstream turbidity 12.3",
        "sub-alice",
    )
    .await;
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

    let reply = commands::grab(
        &state,
        &AccessScope::Unrestricted,
        "upstream turbidity 10 11 12",
        "sub-alice",
    )
    .await;
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

    let reply = commands::grab(
        &state,
        &AccessScope::Unrestricted,
        "nosuchplace turbidity 5",
        "sub-alice",
    )
    .await;
    assert!(reply.contains("No site matches"), "reply: {reply}");
}

#[tokio::test]
#[serial]
async fn grab_rejects_non_numeric_value() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let reply = commands::grab(
        &state,
        &AccessScope::Unrestricted,
        "upstream turbidity oops",
        "sub-alice",
    )
    .await;
    assert!(reply.contains("not a number"), "reply: {reply}");
}

/// Provenance is the Keycloak identity. A Telegram handle can be changed and reassigned, so it is
/// an address, never an identity.
#[tokio::test]
#[serial]
async fn a_grab_is_attributed_to_the_keycloak_identity() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    commands::grab(
        &state,
        &AccessScope::Unrestricted,
        "upstream turbidity 44.4",
        "sub-alice",
    )
    .await;

    let created_by: Option<String> = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT created_by FROM samples ORDER BY created_at DESC LIMIT 1".to_string(),
        ))
        .await
        .unwrap()
        .expect("a sample")
        .try_get("", "created_by")
        .unwrap();
    assert_eq!(created_by.as_deref(), Some("keycloak:sub-alice"));
}

/// Flagging a field submission must reach only the rows that submission created.
#[tokio::test]
#[serial]
async fn flagging_a_grab_leaves_neighbouring_readings_alone() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let mut state = crate::common::build_test_app_with_state(db.clone()).1;
    {
        let config = std::sync::Arc::make_mut(&mut state.config);
        config.telegram_grab_flag_for_review = true;
    }

    // A pre-existing reading in the same slot, which the submission must not touch.
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value) \
             SELECT (SELECT id FROM data_streams ORDER BY id LIMIT 1), '{site}', '{param}', \
                    NOW(), 999.0",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    )
    .await;

    commands::grab(
        &state,
        &AccessScope::Unrestricted,
        "upstream turbidity 55.5",
        "sub-alice",
    )
    .await;

    let neighbour_flagged: bool = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT COALESCE(bool_or(is_flagged), false) AS f FROM readings WHERE raw_value = 999.0"
                .to_string(),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "f")
        .unwrap();
    assert!(
        !neighbour_flagged,
        "a grab must not flag a reading it did not create"
    );

    let submission_flagged: bool = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT COALESCE(bool_or(is_flagged), false) AS f FROM readings WHERE raw_value = 55.5"
                .to_string(),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "f")
        .unwrap();
    assert!(submission_flagged, "its own reading is flagged");
}
