//! The `notification_subscribers` entity: the roster's device count, and what a preference update
//! writes. Both need Postgres, the count because it is one grouped statement over the device table
//! and the update because the point of it is which columns the row keeps.
//!
//! Run: cargo test --test notifications subscriber_entity -- --test-threads=1

use river_db::routes::private::notifications::models::{push_subscription, subscriber};
use river_db::routes::private::notifications::service::push_device_counts;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter};
use serial_test::serial;
use uuid::Uuid;

async fn add_subscriber(db: &sea_orm::DatabaseConnection, sub: &str) -> subscriber::Model {
    subscriber::Entity::insert(subscriber::ActiveModel {
        id: Set(Uuid::new_v4()),
        keycloak_sub: Set(sub.to_string()),
        ..Default::default()
    })
    .exec_with_returning(db)
    .await
    .expect("insert a subscriber")
}

async fn add_device(db: &sea_orm::DatabaseConnection, sub: &str, endpoint: &str) {
    push_subscription::Entity::insert(push_subscription::ActiveModel {
        id: Set(Uuid::new_v4()),
        keycloak_sub: Set(sub.to_string()),
        endpoint: Set(endpoint.to_string()),
        p256dh: Set("key".to_string()),
        auth: Set("auth".to_string()),
        user_agent: Set(None),
        ..Default::default()
    })
    .exec(db)
    .await
    .expect("insert a push device");
}

#[tokio::test]
#[serial]
async fn the_roster_counts_each_persons_devices() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    add_subscriber(&db, "sub-with-devices").await;
    add_subscriber(&db, "sub-without").await;
    add_device(&db, "sub-with-devices", "https://push.example/aaa").await;
    add_device(&db, "sub-with-devices", "https://push.example/bbb").await;
    add_device(&db, "sub-elsewhere", "https://push.example/ccc").await;

    let counts = push_device_counts(
        &db,
        &["sub-with-devices".to_string(), "sub-without".to_string()],
    )
    .await
    .expect("count devices");

    assert_eq!(counts.get("sub-with-devices"), Some(&2));
    assert_eq!(
        counts.get("sub-without"),
        None,
        "a person with no device is absent, which the roster reads as zero"
    );
    assert_eq!(
        counts.get("sub-elsewhere"),
        None,
        "only the logins asked about are counted"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// Expected behaviour: a preference update writes the preferences it was given and leaves every
/// other column as it stands, so a second preference cannot be reset by an update that never
/// mentions it.
#[tokio::test]
#[serial]
async fn an_update_leaves_the_preferences_it_does_not_name() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let row = add_subscriber(&db, "sub-prefs").await;
    let created_at = row.created_at;

    let mut prefs = row.into_active_model();
    prefs.web_push_enabled = Set(false);
    prefs.updated_at = Set(chrono::Utc::now());
    sea_orm::ActiveModelTrait::update(prefs, &db)
        .await
        .expect("turn web push off");

    let stored = subscriber::Entity::find()
        .filter(subscriber::Column::KeycloakSub.eq("sub-prefs"))
        .one(&db)
        .await
        .expect("read the row back")
        .expect("the row is there");
    assert!(!stored.web_push_enabled);
    assert_eq!(stored.created_at, created_at, "creation is not rewritten");

    let mut touched = stored.into_active_model();
    touched.updated_at = Set(chrono::Utc::now());
    sea_orm::ActiveModelTrait::update(touched, &db)
        .await
        .expect("touch the row without naming a preference");

    let stored = subscriber::Entity::find()
        .filter(subscriber::Column::KeycloakSub.eq("sub-prefs"))
        .one(&db)
        .await
        .expect("read the row back")
        .expect("the row is there");
    assert!(
        !stored.web_push_enabled,
        "an update naming no preference leaves the switch off"
    );

    crate::common::cleanup_test_db(&db).await;
}
