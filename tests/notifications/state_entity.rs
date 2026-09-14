//! `notification_state` is keyed `(kind, subject_key)`. What proves its entity is Postgres: the
//! derived columns have to match the live table, and the composite key has to select one row.
//!
//! Run: cargo test --test notifications state_entity -- --test-threads=1

use river_db::routes::private::notifications::models::state;
use sea_orm::{ActiveValue, ColumnTrait, EntityTrait, QueryFilter};
use serial_test::serial;

async fn insert(db: &sea_orm::DatabaseConnection, kind: &str, subject_key: &str, value: &str) {
    state::Entity::insert(state::ActiveModel {
        kind: ActiveValue::Set(kind.to_string()),
        subject_key: ActiveValue::Set(subject_key.to_string()),
        state: ActiveValue::Set(value.to_string()),
        last_notified_at: ActiveValue::Set(chrono::Utc::now()),
        detail: ActiveValue::Set(None),
    })
    .exec(db)
    .await
    .expect("insert a state row");
}

#[tokio::test]
#[serial]
async fn the_composite_key_selects_one_row() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    insert(&db, "channel_health", "web_push", "healthy").await;
    insert(&db, "channel_health", "telegram", "unhealthy").await;
    insert(&db, "stale_data", "web_push", "firing").await;

    let row = state::Entity::find()
        .filter(state::Column::Kind.eq("channel_health"))
        .filter(state::Column::SubjectKey.eq("web_push"))
        .one(&db)
        .await
        .expect("read one state row")
        .expect("the row is there");
    assert_eq!(row.state, "healthy");
    assert!(row.detail.is_none());

    let of_kind = state::Entity::find()
        .filter(state::Column::Kind.eq("channel_health"))
        .all(&db)
        .await
        .expect("read a kind");
    assert_eq!(of_kind.len(), 2, "one row per subject key under the kind");

    let cleared = state::Entity::delete_many()
        .filter(state::Column::Kind.eq("channel_health"))
        .filter(state::Column::SubjectKey.eq("web_push"))
        .exec(&db)
        .await
        .expect("clear one state row");
    assert_eq!(
        cleared.rows_affected, 1,
        "the other subject key is untouched"
    );

    crate::common::cleanup_test_db(&db).await;
}
