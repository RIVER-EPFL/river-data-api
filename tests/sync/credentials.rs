//! Enrollment credential minting and revocation.
//!
//! `POST /api/sync/credentials` and `/credentials/{id}/revoke` are `require_admin`, so the
//! authorization gate is covered by the Keycloak suite. Here the handlers are called directly
//! against the same `AppState` the HTTP app holds, which is what lets the behaviour be asserted
//! without Keycloak: the shape of a minted credential, and that a revoked one can no longer
//! enroll or keep a live session.
//!
//! Run: cargo test --test sync -- --test-threads=1

use axum::Json;
use axum::extract::{Path, State};
use river_db::routes::private::sync::models::CreateCredentialRequest;
use river_db::routes::private::sync::views::create_credential;
use river_db::routes::private::sync::views::revoke_credential;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use river_db::routes::private::api_tokens::service::hash_token;

async fn scalar(db: &DatabaseConnection, sql: &str) -> String {
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            sql.to_string(),
        ))
        .await
        .expect("query")
        .expect("row");
    row.try_get::<String>("", "v").expect("v")
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            sql.to_string(),
        ))
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
            source_system: Some("vaisala".to_string()),
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
        minted
            .client_secret
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
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
    assert!(
        stored.starts_with("$argon2id$"),
        "stored salted and work-factored, not as a digest a wordlist covers in one pass: {stored}"
    );
    assert_ne!(stored, hash_token(&minted.client_secret));
    assert_ne!(stored, minted.client_secret);
    assert!(
        river_db::routes::private::api_tokens::service::verify_api_secret(
            &minted.client_secret,
            &stored
        ),
        "the minted secret verifies against what was stored"
    );

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
            source_system: Some("vaisala".to_string()),
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
    assert_eq!(
        status, 200,
        "enroll with minted credentials ({status}): {body}"
    );
}

/// Scenario: a credential issued before enrollment secrets were argon2-hashed, so the row holds an
/// unsalted SHA-256 digest and the plaintext is held nowhere.
///
/// Expected behaviour: it still enrolls, because breaking every deployed service is not an upgrade
/// path, and the enrollment is the one moment the plaintext is in hand, so the row is rewritten as
/// argon2 there. The second enrollment then verifies against the new hash, and the secret is not
/// re-issued.
#[tokio::test]
#[serial]
async fn a_credential_stored_as_a_bare_digest_enrolls_and_is_rehashed() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (app, _state) = crate::common::build_test_app_with_state(db.clone());

    let secret = "a".repeat(64);
    let client_id = "svc_legacydigest0001";
    db.execute_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "INSERT INTO sync_service_credentials \
             (id, client_id, client_secret_hash, service_type, revoked, created_at) \
             VALUES ('{}', '{client_id}', '{}', 'vaisala', false, NOW())",
            Uuid::new_v4(),
            hash_token(&secret)
        ),
    ))
    .await
    .expect("seed a legacy credential");

    async fn enroll(
        app: &axum::Router,
        client_id: &str,
        secret: &str,
        instance: &str,
    ) -> (u16, String) {
        crate::common::post_json(
            app,
            "/api/sync/enroll",
            &serde_json::json!({
                "client_id": client_id,
                "client_secret": secret,
                "instance_id": instance,
            }),
        )
        .await
    }

    let (status, body) = enroll(&app, client_id, &secret, "inst-legacy-1").await;
    assert_eq!(status, 200, "a legacy credential still enrolls: {body}");

    let stored = scalar(
        &db,
        &format!(
            "SELECT client_secret_hash AS v FROM sync_service_credentials \
             WHERE client_id = '{client_id}'"
        ),
    )
    .await;
    assert!(
        stored.starts_with("$argon2id$"),
        "the enrollment rehashed it: {stored}"
    );

    let (status, body) = enroll(&app, client_id, &secret, "inst-legacy-2").await;
    assert_eq!(
        status, 200,
        "the same secret enrolls against the new hash: {body}"
    );

    let (status, _) = enroll(&app, client_id, &"b".repeat(64), "inst-legacy-3").await;
    assert_eq!(
        status, 401,
        "a wrong secret is still refused after the rehash"
    );
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
            source_system: Some("cnet".to_string()),
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
    assert!(revoked.revoked);

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
            &format!(
                "SELECT COUNT(*) AS c FROM sync_service_tokens WHERE service_id = '{service_id}'"
            )
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

/// Scenario: two rshiny services enroll on their own credentials, one for CNET and one for METALP.
///
/// Expected behaviour: the source system each speaks for is read back from its session token.
/// `service_type` cannot answer this, it is `rshiny` for both; before the declaration existed the
/// only statement of the source system was a string in each register call's body, so the server
/// could not tell the two services apart at all.
#[tokio::test]
#[serial]
async fn a_service_speaks_for_the_source_system_its_credential_declares() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (app, state) = crate::common::build_test_app_with_state(db.clone());

    let mut sessions = Vec::new();
    for source in ["cnet", "metalp"] {
        let Json(minted) = create_credential(
            State(state.clone()),
            Json(CreateCredentialRequest {
                service_type: "rshiny".to_string(),
                source_system: Some(source.to_string()),
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
                "instance_id": format!("inst-{source}"),
            }),
        )
        .await;
        assert_eq!(status, 200, "enroll {source} ({status}): {body}");
        let enrolled: serde_json::Value =
            serde_json::from_str(&body).expect("the enrollment response is JSON");
        let token = enrolled["session_token"]
            .as_str()
            .expect("a session token")
            .to_string();
        sessions.push((source, token));
    }

    for (source, token) in sessions {
        let session = river_db::routes::private::sync::service::lookup_sync_session(&db, &token)
            .await
            .expect("a live session");
        assert_eq!(
            session.source_system.as_deref(),
            Some(source),
            "the session says which source system the service speaks for"
        );
    }

    // The kind of service is the same word for both, which is why it cannot carry the source.
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) AS c FROM sync_services WHERE service_type = 'rshiny'"
        )
        .await,
        2
    );
}

/// Scenario: a portal sync service enrolled for METALP registers a stream claiming CNET's
/// provenance.
///
/// Expected behaviour: refused, and nothing is written under the claimed system.
/// `(source_system, source_key)` is the key a source holds its own rows by and the registration
/// upserts on it, so an accepted claim would take over the other portal's stream, pairing and all.
/// A service that names no system at all is fine: its declaration answers the question.
#[tokio::test]
#[serial]
async fn a_service_cannot_register_another_portals_stream() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (app, _state) = crate::common::build_test_app_with_state(db.clone());
    let (metalp_token, _) =
        crate::common::seed_sync_session_token_declaring(&db, Some("metalp")).await;

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &serde_json::json!({
            "source_system": "cnet",
            "source_key": "S01:DOC_ppb",
            "source_name": "DOC at S01",
        }),
        &metalp_token,
    )
    .await;
    assert_eq!(status, 403, "claiming another source ({status}): {body}");
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*)::bigint AS c FROM data_streams WHERE source_system = 'cnet'"
        )
        .await,
        0,
        "nothing was written under the claimed system"
    );

    // Naming none, the declaration is what the row is keyed under.
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &serde_json::json!({
            "source_system": "",
            "source_key": "S01:DOC_ppb",
            "source_name": "DOC at S01",
        }),
        &metalp_token,
    )
    .await;
    assert_eq!(status, 200, "its own source ({status}): {body}");
    assert_eq!(body["source_system"], "metalp");
}
