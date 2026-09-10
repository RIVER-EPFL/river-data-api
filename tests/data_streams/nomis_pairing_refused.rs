//! NOMIS streams are not paired while their timezone is unconfirmed.
//!
//! The portal reports a plain date and a plain time with no zone column, and the connector reads
//! them as UTC. ADR 0004 makes that an assumption to confirm before any NOMIS data is paired, and
//! pairing is where it would become attributed data.
//!
//! Run: cargo test --test data_streams nomis_pairing_refused -- --test-threads=1

use serde_json::json;
use serial_test::serial;

async fn register(app: &axum::Router, token: &str, source_system: &str, key: &str) -> String {
    let (status, stream) = crate::common::post_json_parse_with_token(
        app,
        "/api/streams/register",
        &json!({"source_system": source_system, "source_key": key}),
        token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "register ({status}): {stream}"
    );
    crate::common::e2e::id_of(&stream)
}

#[tokio::test]
#[serial]
async fn a_nomis_stream_is_refused_and_a_portal_stream_is_not() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let nomis = register(&app, &token, "nomis", "GL1:DOC").await;
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{nomis}/pair"),
        &json!({"site_parameter_id": crate::common::PARAM_S1_TEMP_ID}),
        &token,
    )
    .await;
    assert_eq!(status, 400, "NOMIS pairing is refused: {body}");
    assert!(
        body.to_string().contains("ADR 0004"),
        "and says which question is open: {body}"
    );

    let cnet = register(&app, &token, "cnet", "FP1:DOC_avg_ppb").await;
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{cnet}/pair"),
        &json!({"site_parameter_id": crate::common::PARAM_S1_TEMP_ID}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "every other source still pairs: {body}");
}
