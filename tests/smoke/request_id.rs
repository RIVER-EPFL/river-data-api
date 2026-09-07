//! The request id every response carries: minted when the caller sends none, kept when it does.

use axum::body::Body;
use serial_test::serial;
use tower::ServiceExt;

async fn request_id_of(app: &axum::Router, sent: Option<&str>) -> Option<String> {
    let mut req = axum::http::Request::builder().method("GET").uri("/healthz");
    if let Some(id) = sent {
        req = req.header("x-request-id", id);
    }
    let response = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    response
        .headers()
        .get("x-request-id")
        .map(|v| v.to_str().unwrap().to_string())
}

#[tokio::test]
#[serial]
async fn a_caller_s_own_request_id_is_the_one_that_comes_back() {
    let db = crate::common::setup_test_db().await;
    let app = crate::common::build_test_app(db);

    let minted = request_id_of(&app, None)
        .await
        .expect("a response with no inbound id still carries one");
    assert!(
        uuid::Uuid::parse_str(&minted).is_ok(),
        "a minted id should be a uuid, got {minted}"
    );
    assert_ne!(
        minted,
        request_id_of(&app, None).await.unwrap(),
        "two requests should not share an id"
    );

    assert_eq!(
        request_id_of(&app, Some("from-the-caller"))
            .await
            .as_deref(),
        Some("from-the-caller"),
        "an inbound id is kept, so a caller can correlate its own trace"
    );
}
