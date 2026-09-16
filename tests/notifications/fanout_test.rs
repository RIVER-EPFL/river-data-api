//! Per-user Web Push fan-out: an alert reaches only push subscriptions whose subscriber has
//! web_push enabled AND is subscribed to that slot. A subscription with no subscriber row defaults
//! to enabled + subscribed. A site-level "off" override suppresses that site only; a system-wide
//! alert (no slot) ignores per-slot overrides.

use river_db::routes::private::notifications::models::Slot;
use river_db::routes::private::notifications::service::slot_subscriptions;
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
            "INSERT INTO notification_subscriptions (keycloak_sub, channel, site_id, enabled) \
             VALUES ('{sub}', 'alarm_opened', '{site}', FALSE)",
            site = crate::common::SITE1_ID,
        ),
    )
    .await;
}

fn endpoints(
    subs: &[river_db::routes::private::notifications::service::Subscription],
) -> Vec<String> {
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

    let scoped = slot_subscriptions(&db, &Some(slot), "alarm_opened")
        .await
        .unwrap();
    assert_eq!(
        endpoints(&scoped),
        vec!["https://push.example.com/a"],
        "only the subscribed, push-enabled endpoint"
    );

    let all = slot_subscriptions(&db, &None, "test").await.unwrap();
    assert_eq!(
        endpoints(&all),
        vec!["https://push.example.com/a", "https://push.example.com/b"],
        "system-wide ignores per-slot overrides"
    );
}

/// The two groups have their own audiences and their own default: a subscriber with no rows at all
/// receives the alarm group and not the sync group, and turning the sync group off leaves the
/// alarms alone.
#[tokio::test]
#[serial]
async fn channels_default_alarms_on_and_the_rest_off() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    push_sub(&db, "sub-quiet", "https://push.example.com/quiet").await;
    push_sub(&db, "sub-loud", "https://push.example.com/loud").await;
    // Only "loud" asks for the sync-silence channel, and it turns alarms off site-wide.
    crate::common::exec(
        &db,
        "INSERT INTO notification_subscriptions (keycloak_sub, channel, enabled) \
         VALUES ('sub-loud', 'sync_stale', TRUE)",
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO notification_subscriptions (keycloak_sub, channel, site_id, enabled) \
             VALUES ('sub-loud', 'alarm_opened', '{site}', FALSE)",
            site = crate::common::SITE1_ID,
        ),
    )
    .await;

    let slot = Slot {
        project_id: None,
        site_id: crate::common::SITE1_ID.parse().unwrap(),
        parameter_id: crate::common::GLOBAL_PARAM_TURB_ID.parse().unwrap(),
    };

    let alarms = slot_subscriptions(&db, &Some(slot.clone()), "alarm_opened")
        .await
        .unwrap();
    assert_eq!(
        endpoints(&alarms),
        vec!["https://push.example.com/quiet"],
        "alarms reach the subscriber with no rows and skip the one who turned this site off"
    );

    let sync_slot = slot_subscriptions(&db, &Some(slot), "sync_stale")
        .await
        .unwrap();
    assert_eq!(
        endpoints(&sync_slot),
        vec!["https://push.example.com/loud"],
        "a sync channel reaches only who asked for it, and the alarm mute does not apply"
    );

    let sync_wide = slot_subscriptions(&db, &None, "sync_stale").await.unwrap();
    assert_eq!(
        endpoints(&sync_wide),
        vec!["https://push.example.com/loud"],
        "a system-wide sync alert answers to the channel-wide row"
    );
}

/// Scenario: a river member and a manager both hold a stored `holds_open` row, and an intern's
/// role cannot be resolved.
/// Expected behaviour: the review queue reaches the manager alone.
#[tokio::test]
#[serial]
async fn holds_open_reaches_only_managers_and_up() {
    use river_db::common::authz::Role;
    use river_db::routes::private::notifications::models::RoleResolution;
    use river_db::routes::private::notifications::service::within_audience;

    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    for sub in ["sub-river", "sub-manager", "sub-unresolved"] {
        push_sub(&db, sub, &format!("https://push.example.com/{sub}")).await;
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO notification_subscriptions (keycloak_sub, channel, enabled) \
                 VALUES ('{sub}', 'holds_open', TRUE)"
            ),
        )
        .await;
    }
    state
        .authorizer
        .prime("sub-river", RoleResolution::Active(Role::River))
        .await;
    state
        .authorizer
        .prime("sub-manager", RoleResolution::Active(Role::Manager))
        .await;

    let reached = slot_subscriptions(&db, &None, "holds_open").await.unwrap();
    assert_eq!(reached.len(), 3, "all three hold a stored row");
    let (admitted, mut refused) = within_audience(&state, "holds_open", reached).await;
    refused.sort();
    assert_eq!(
        endpoints(&admitted),
        vec!["https://push.example.com/sub-manager"]
    );
    assert_eq!(refused, vec!["sub-river", "sub-unresolved"]);
}
