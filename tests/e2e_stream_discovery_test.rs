//! End-to-end stream management & reconciliation: register source-agnostic streams, view the
//! discovery report, pair a stream to a site_parameter (with backfill) and unpair it, read stream
//! stats, and confirm a sync service surfaces in the sync health view (US-11.1/11.2/11.4).
//!
//! Note: the full auto-discover → apply-discovery → batch-create path (US-11.3) is driven by the
//! sync microservice against real Vaisala source paths; here we exercise the manual pairing path
//! and confirm the discovery endpoint responds.
//!
//! Run: cargo test --test e2e_stream_discovery_test -- --test-threads=1

mod common;

use common::e2e;
use common::sensor_lifecycle as sl;
use serial_test::serial;

#[tokio::test]
#[serial]
async fn register_discover_pair_unpair_and_sync_health() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await; // site_parameters exist but UNPAIRED (no seed streams)
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());

    // US-11.1: register a source-agnostic stream; it starts unpaired.
    let (status, stream) = common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &serde_json::json!({ "source_system": "e2e", "source_key": "disc-1", "source_name": "Discovery stream 1" }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "register ({status}): {stream}");
    assert!(stream["site_parameter_id"].is_null(), "new stream is unpaired: {stream}");
    let stream_id = e2e::id_of(&stream);

    // US-11.2: the discovery report responds (suggestions depend on source-path conventions).
    let (status, discovery) = common::get_json_with_token(&app, "/api/sync/discovery", &token).await;
    assert_eq!(status, 200, "discovery ({status}): {discovery}");
    assert!(discovery.is_object() || discovery.is_array(), "discovery returns a structured report");
    // The unpaired-summary should reflect our unpaired stream.
    let (status, summary) = common::get_json_with_token(&app, "/api/sync/unpaired-summary", &token).await;
    assert_eq!(status, 200, "unpaired-summary ({status}): {summary}");

    // US-11.1: pair the stream to a site_parameter, then unpair it.
    let (status, paired) = common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &serde_json::json!({ "site_parameter_id": common::PARAM_S1_TEMP_ID }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "pair ({status}): {paired}");
    assert!(!paired["stream"]["site_parameter_id"].is_null(), "stream is now paired: {paired}");

    let (status, stats) = common::get_json_with_token(&app, &format!("/api/streams/{stream_id}/stats"), &token).await;
    assert_eq!(status, 200, "stream stats ({status}): {stats}");

    let (status, cleared) = common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/unpair"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "unpair ({status}): {cleared}");

    // US-11.4: a registered sync service appears in the sync health view.
    let (_token2, service_id) = common::seed_sync_session_token(&db).await;
    let (status, services) = common::get_with_token(&app, "/api/sync/services", &token).await;
    assert_eq!(status, 200, "sync services ({status})");
    assert!(
        services.contains(&service_id.to_string()),
        "the seeded sync service {service_id} should appear in /sync/services: {services}"
    );
}
