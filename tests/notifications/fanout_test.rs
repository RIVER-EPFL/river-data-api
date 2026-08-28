//! Per-user Web Push fan-out: an alert reaches only push subscriptions whose subscriber has
//! web_push enabled AND is subscribed to that slot. A subscription with no subscriber row defaults
//! to enabled + subscribed. A site-level "off" override suppresses that site only; a system-wide
//! alert (no slot) ignores per-slot overrides.

use river_db::routes::private::notifications::{Slot, web_push::slot_subscriptions};
use sea_orm::DatabaseConnection;
use serial_test::serial;

async fn push_sub(db: &DatabaseConnection, sub: &str, endpoint: &str) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO web_push_subscriptions (keycloak_sub, endpoint, p256dh, auth) \
             VALUES ('{sub}', '{endpoint}', 'key', 'auth')"
        ),
    )
    .await;
}

async fn subscriber(db: &DatabaseConnection, sub: &str, web_push_enabled: bool) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO notification_subscribers (keycloak_sub, web_push_enabled) \
             VALUES ('{sub}', {web_push_enabled})"
        ),
    )
    .await;
}

async fn mute_site(db: &DatabaseConnection, sub: &str) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO notification_subscriptions (keycloak_sub, site_id, enabled) \
             VALUES ('{sub}', '{site}', FALSE)",
            site = crate::common::SITE1_ID,
        ),
    )
    .await;
}

fn endpoints(subs: &[river_db::routes::private::notifications::web_push::Subscription]) -> Vec<String> {
    let mut eps: Vec<String> = subs.iter().map(|s| s.endpoint.clone()).collect();
    eps.sort();
    eps
}

#[tokio::test]
#[serial]
async fn web_push_fanout_respects_subscription_and_channel_toggle() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    // A: subscribed, no subscriber row → default enabled.
    push_sub(&db, "sub-a", "https://push.example.com/a").await;
    // B: subscribed, push on, but muted this site.
    push_sub(&db, "sub-b", "https://push.example.com/b").await;
    subscriber(&db, "sub-b", true).await;
    mute_site(&db, "sub-b").await;
    // C: subscribed, push disabled.
    push_sub(&db, "sub-c", "https://push.example.com/c").await;
    subscriber(&db, "sub-c", false).await;

    let slot = Slot {
        project_id: None,
        site_id: crate::common::SITE1_ID.parse().unwrap(),
        parameter_id: crate::common::GLOBAL_PARAM_TURB_ID.parse().unwrap(),
    };

    let scoped = slot_subscriptions(&db, &Some(slot)).await.unwrap();
    assert_eq!(
        endpoints(&scoped),
        vec!["https://push.example.com/a"],
        "only the subscribed, push-enabled endpoint"
    );

    let all = slot_subscriptions(&db, &None).await.unwrap();
    assert_eq!(
        endpoints(&all),
        vec!["https://push.example.com/a", "https://push.example.com/b"],
        "system-wide ignores per-slot overrides"
    );
}
