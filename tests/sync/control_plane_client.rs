//! The control plane over the wire, driven by the client every sync service actually runs.
//!
//! Every other enrolment test writes the request body itself, so the two halves of the protocol
//! are each asserted against a copy of the other. Here `river_data_core`'s `ControlPlaneClient`
//! talks to the real router on a loopback port: a field renamed on either side fails here.
//!
//! Run: cargo test --test sync control_plane_client -- --test-threads=1

use axum::Json;
use axum::extract::State;
use river_data_core::client::ControlPlaneClient;
use river_data_core::models::ServiceStatus;
use river_db::routes::private::sync::models::CreateCredentialRequest;
use river_db::routes::private::sync::views::create_credential;
use serial_test::serial;

#[tokio::test]
#[serial]
async fn the_shipped_client_enrolls_and_heartbeats_against_the_real_router() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (app, state) = crate::common::build_test_app_with_state(db.clone());

    let Json(minted) = create_credential(
        State(state.clone()),
        Json(CreateCredentialRequest {
            service_type: "cnet".to_string(),
        }),
    )
    .await
    .expect("mint an enrolment credential");

    let base = crate::common::serve(app).await;
    let mut client = ControlPlaneClient::new(&base).expect("build the client");

    let enrolled = client
        .enroll(&minted.client_id, &minted.client_secret, "inst-core-client")
        .await
        .expect("enroll through the shipped client");
    assert!(
        !enrolled.session_token.is_empty(),
        "enrolment returns the session token the heartbeat authenticates with"
    );

    // The session the enrolment returned is the one the heartbeat is accepted on, and the
    // service's own settings come back on it.
    let beat = client
        .heartbeat(enrolled.service_id, ServiceStatus::Running, Some("idle"))
        .await
        .expect("heartbeat on the enrolled session");
    assert!(
        !beat.paused,
        "a freshly enrolled service is not paused: {beat:?}"
    );

    let stored: i64 = crate::common::e2e::count(
        &db,
        &format!(
            "SELECT COUNT(*)::bigint FROM sync_services \
             WHERE id = '{}' AND instance_id = 'inst-core-client'",
            enrolled.service_id
        ),
    )
    .await;
    assert_eq!(stored, 1, "the enrolment registered one service");
}
