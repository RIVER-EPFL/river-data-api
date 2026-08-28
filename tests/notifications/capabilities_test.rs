//! `GET /api/config/notifications` reports which channels the deployment has configured (env-gated)
//! so the frontend can enable/disable the push subscription UI. It answers without auth and leaks
//! no secrets, the payload carries only an availability boolean and the public VAPID key.

use serial_test::serial;

#[tokio::test]
#[serial]
async fn capabilities_report_disabled_and_leak_no_secrets() {
    let db = crate::common::setup_test_db().await;
    let app = crate::common::build_test_app(db);

    let (status, body) = crate::common::get_json(&app, "/api/config/notifications").await;
    assert_eq!(
        status, 200,
        "capabilities endpoint answers (no auth, no 404)"
    );

    assert_eq!(body["webPush"]["available"], false);

    let obj = body.as_object().expect("object payload");
    assert_eq!(obj.len(), 1, "only webPush");
    let wp = body["webPush"].as_object().unwrap();
    assert!(
        wp.keys().all(|k| k == "available" || k == "vapidPublicKey"),
        "webPush keys limited to available/vapidPublicKey, got {:?}",
        wp.keys().collect::<Vec<_>>()
    );
}
