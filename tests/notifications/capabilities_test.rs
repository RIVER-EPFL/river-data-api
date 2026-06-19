//! `GET /api/config/notifications` reports which channels the deployment has configured (env-gated)
//! so the frontend can grey out unavailable ones. It answers without auth and leaks no secrets — the
//! payload is exactly availability booleans + the email backend kind + an optional public bot username.
//!
//! Run: cargo test --test notifications -- --test-threads=1

use serial_test::serial;

#[tokio::test]
#[serial]
async fn capabilities_report_disabled_and_leak_no_secrets() {
    let db = crate::common::setup_test_db().await;
    let app = crate::common::build_test_app(db);

    let (status, body) = crate::common::get_json(&app, "/api/config/notifications").await;
    assert_eq!(status, 200, "capabilities endpoint answers (no auth, no 404)");

    assert_eq!(body["telegram"]["available"], false);
    assert_eq!(body["email"]["available"], false);
    assert_eq!(body["email"]["backend"], "disabled");

    // Exact key shape — guards against any credential field ever leaking into the payload.
    let obj = body.as_object().expect("object payload");
    assert_eq!(obj.len(), 2, "only telegram + email");
    let tg = body["telegram"].as_object().unwrap();
    assert!(
        tg.keys().all(|k| k == "available" || k == "botUsername"),
        "telegram keys limited to available/botUsername, got {:?}",
        tg.keys().collect::<Vec<_>>()
    );
    let em = body["email"].as_object().unwrap();
    assert_eq!(em.len(), 2, "email keys are exactly available + backend");
    assert!(em.contains_key("available") && em.contains_key("backend"));
}
