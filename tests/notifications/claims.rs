//! The dispatcher's multi-replica claims on `notification_state`: the conflict action's own
//! `WHERE` decides the winner, so two replicas claiming one key send one alert, not two or none.
//!
//! Run: cargo test --test notifications claims -- --test-threads=1

use chrono::{Duration, Utc};
use river_db::routes::private::notifications::models::state;
use river_db::routes::private::notifications::service::{
    claim_cas, claim_clear, claim_insert, claim_renotify,
};
use sea_orm::{ActiveValue, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serial_test::serial;

async fn stamp(db: &DatabaseConnection, kind: &str, key: &str, ago: Duration) {
    state::Entity::insert(state::ActiveModel {
        kind: ActiveValue::Set(kind.to_string()),
        subject_key: ActiveValue::Set(key.to_string()),
        state: ActiveValue::Set("firing".to_string()),
        last_notified_at: ActiveValue::Set(Utc::now() - ago),
        detail: ActiveValue::Set(None),
    })
    .exec(db)
    .await
    .expect("stamp a state row");
}

async fn stored_at(db: &DatabaseConnection, kind: &str, key: &str) -> chrono::DateTime<Utc> {
    state::Entity::find()
        .filter(state::Column::Kind.eq(kind))
        .filter(state::Column::SubjectKey.eq(key))
        .one(db)
        .await
        .expect("read the row")
        .expect("the row stands")
        .last_notified_at
}

#[tokio::test]
#[serial]
async fn two_replicas_claiming_one_key_send_once() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let (a, b) = tokio::join!(
        claim_insert(&db, "stale_data", "s:p:spot"),
        claim_insert(&db, "stale_data", "s:p:spot")
    );
    let wins = [a.unwrap(), b.unwrap()];
    assert_eq!(wins.iter().filter(|w| **w).count(), 1, "{wins:?}");
    assert!(!claim_insert(&db, "stale_data", "s:p:spot").await.unwrap());

    assert!(claim_clear(&db, "stale_data", "s:p:spot").await.unwrap());
    assert!(!claim_clear(&db, "stale_data", "s:p:spot").await.unwrap());

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn a_renotify_waits_out_its_window() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    assert!(claim_renotify(&db, "holds_open", "all", 6).await.unwrap());
    assert!(
        !claim_renotify(&db, "holds_open", "all", 6).await.unwrap(),
        "inside the window nobody re-notifies"
    );

    stamp(&db, "sync_stale", "cnet", Duration::hours(7)).await;
    let before = stored_at(&db, "sync_stale", "cnet").await;
    let (a, b) = tokio::join!(
        claim_renotify(&db, "sync_stale", "cnet", 6),
        claim_renotify(&db, "sync_stale", "cnet", 6)
    );
    let wins = [a.unwrap(), b.unwrap()];
    assert_eq!(wins.iter().filter(|w| **w).count(), 1, "{wins:?}");
    assert!(
        stored_at(&db, "sync_stale", "cnet").await > before,
        "the winner advanced the stamp"
    );

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn a_watermark_advances_once_per_value_read() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let since = Utc::now() - Duration::hours(1);
    assert!(
        claim_cas(&db, "curve_drift", "all", since).await.unwrap(),
        "absent row: first claim wins"
    );
    let read = stored_at(&db, "curve_drift", "all").await;
    assert!(
        !claim_cas(&db, "curve_drift", "all", since).await.unwrap(),
        "a stale value loses"
    );

    let (a, b) = tokio::join!(
        claim_cas(&db, "curve_drift", "all", read),
        claim_cas(&db, "curve_drift", "all", read)
    );
    let wins = [a.unwrap(), b.unwrap()];
    assert_eq!(wins.iter().filter(|w| **w).count(), 1, "{wins:?}");

    crate::common::cleanup_test_db(&db).await;
}
