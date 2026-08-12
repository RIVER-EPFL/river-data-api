//! Enrollment credential minting and revocation.
//!
//! `POST /api/sync/credentials` and `/credentials/{id}/revoke` are `require_admin`, so the
//! authorization gate is covered by the Keycloak suite. Here the handlers are called directly
//! against the same `AppState` the HTTP app holds, which is what lets the behaviour be asserted
//! without Keycloak: the shape of a minted credential, and that a revoked one can no longer
//! enroll or keep a live session.
//!
//! Run: cargo test --test sync -- --test-threads=1

use axum::extract::{Path, State};
use axum::Json;
use river_db::routes::private::sync::operator::{
    create_credential, revoke_credential, CreateCredentialRequest,
};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use river_db::routes::private::api_tokens::service::hash_token;

async fn scalar(db: &DatabaseConnection, sql: &str) -> String {
    let row = db
        .query_one(Statement::from_string(DatabaseBackend::Postgres, sql.to_string()))
        .await
        .expect("query")
        .expect("row");
    row.try_get::<String>("", "v").expect("v")
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    let row = db
        .query_one(Statement::from_string(DatabaseBackend::Postgres, sql.to_string()))
        .await
        .expect("query")
        .expect("row");
    row.try_get::<i64>("", "c").expect("c")
}

#[tokio::test]
#[serial]
async fn minted_credentials_enroll_and_are_stored_hashed() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (app, state) = crate::common::build_test_app_with_state(db.clone());

    let Json(minted) = create_credential(
        State(state.clone()),
        Json(CreateCredentialRequest {
            service_type: "vaisala".to_string(),
        }),
    )
    .await
    .expect("mint");

    assert!(
        minted.client_id.starts_with("svc_"),
        "client_id prefix: {}",
        minted.client_id
    );
    assert_eq!(minted.client_id.len(), 20, "svc_ plus 16 hex chars");
    assert_eq!(minted.client_secret.len(), 64, "32 random bytes as hex");
    assert!(
        minted.client_secret.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "secret alphabet: {}",
        minted.client_secret
    );

    // The stored hash is what every enrolled service in the field is matched against.
    let stored = scalar(
        &db,
        &format!(
            "SELECT client_secret_hash AS v FROM sync_service_credentials \
             WHERE client_id = '{}'",
            minted.client_id
        ),
    )
    .await;
    assert_eq!(stored, hash_token(&minted.client_secret));

    let service_type = scalar(
        &db,
        &format!(
            "SELECT service_type AS v FROM sync_service_credentials WHERE client_id = '{}'",
            minted.client_id
        ),
    )
    .await;
    assert_eq!(service_type, "vaisala");
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) AS c FROM sync_service_credentials \
                 WHERE client_id = '{}' AND revoked = false AND service_id IS NULL",
                minted.client_id
            )
        )
        .await,
        1
    );

    let Json(second) = create_credential(
        State(state.clone()),
        Json(CreateCredentialRequest {
            service_type: "vaisala".to_string(),
        }),
    )
    .await
    .expect("mint again");
    assert_ne!(minted.client_id, second.client_id);
    assert_ne!(minted.client_secret, second.client_secret);

    // Closing the loop over HTTP: a changed prefix, hash or alphabet fails here.
    let (status, body) = crate::common::post_json(
        &app,
        "/api/sync/enroll",
        &serde_json::json!({
            "client_id": minted.client_id,
            "client_secret": minted.client_secret,
            "instance_id": "inst-mint-1",
        }),
    )
    .await;
    assert_eq!(status, 200, "enroll with minted credentials ({status}): {body}");
}

#[tokio::test]
#[serial]
async fn revoking_a_credential_kills_enrollment_and_live_sessions() {
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
    .expect("mint");

    let (status, body) = crate::common::post_json(
        &app,
        "/api/sync/enroll",
        &serde_json::json!({
            "client_id": minted.client_id,
            "client_secret": minted.client_secret,
            "instance_id": "inst-revoke-1",
        }),
    )
    .await;
    assert_eq!(status, 200, "enroll ({status}): {body}");
    let enrolled: serde_json::Value = serde_json::from_str(&body).unwrap();
    let session_token = enrolled["session_token"].as_str().unwrap().to_string();
    let service_id = enrolled["service_id"].as_str().unwrap().to_string();

    let credential_id: String = scalar(
        &db,
        &format!(
            "SELECT id::text AS v FROM sync_service_credentials WHERE client_id = '{}'",
            minted.client_id
        ),
    )
    .await;

    let Json(revoked) = revoke_credential(
        State(state.clone()),
        Path(Uuid::parse_str(&credential_id).unwrap()),
    )
    .await
    .expect("revoke");
    assert_eq!(revoked["revoked"], true);

    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) AS c FROM sync_service_credentials \
                 WHERE id = '{credential_id}' AND revoked = true"
            )
        )
        .await,
        1,
        "credential is marked revoked"
    );
    assert_eq!(
        count(
            &db,
            &format!("SELECT COUNT(*) AS c FROM sync_service_tokens WHERE service_id = '{service_id}'")
        )
        .await,
        0,
        "active sessions are terminated"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/sync/heartbeat",
        &serde_json::json!({ "service_id": service_id, "status": "idle" }),
        &session_token,
    )
    .await;
    assert_eq!(status, 401, "the revoked session token is rejected: {body}");

    let (status, body) = crate::common::post_json(
        &app,
        "/api/sync/enroll",
        &serde_json::json!({
            "client_id": minted.client_id,
            "client_secret": minted.client_secret,
            "instance_id": "inst-revoke-1",
        }),
    )
    .await;
    assert_eq!(status, 401, "re-enrolling with revoked credentials: {body}");
}

#[tokio::test]
#[serial]
async fn revoking_an_unknown_credential_is_not_found() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let result = revoke_credential(State(state), Path(Uuid::new_v4())).await;
    assert!(result.is_err(), "an unknown credential id must not succeed");
}
