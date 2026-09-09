use axum::Json;
use axum::extract::State;
use chrono::Utc;
use sea_orm::{ActiveModelTrait, ColumnTrait, Condition, EntityTrait, QueryFilter, Set};
use uuid::Uuid;

use super::heartbeat::SESSION_TOKEN_CACHE;
use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::sync::{credentials_model, services_model, tokens_model};
use river_data_core::models::{EnrollRequest, EnrollResponse, ServiceStatus};
use subtle::ConstantTimeEq;

/// The one answer every refused enrollment gets. A probe that walks client ids must not be able
/// to tell an id that exists from one that does not, so the reason stays server-side.
const ENROLL_DENIED: &str = "Invalid client credentials";

/// Why an enrollment was refused. Logged, never served.
#[derive(Debug, PartialEq, Eq)]
enum EnrollDenial {
    UnknownClient,
    Revoked,
    BadSecret,
}

impl EnrollDenial {
    const fn public_message(&self) -> &'static str {
        ENROLL_DENIED
    }

    const fn reason(&self) -> &'static str {
        match self {
            Self::UnknownClient => "unknown client_id",
            Self::Revoked => "credential revoked",
            Self::BadSecret => "client_secret does not match",
        }
    }
}

/// How a stored credential's secret was hashed, so a credential that predates argon2 can be
/// upgraded in place on the one occasion the plaintext is in hand.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum SecretFormat {
    /// Argon2id PHC string, what `create_credential` mints.
    Argon2,
    /// Unsalted SHA-256 hex, what credentials minted before this stored. Accepted so an enrolled
    /// service keeps working, and rewritten as argon2 the first time it enrolls.
    LegacyDigest,
}

/// A stored hash is argon2 when it is a PHC string; anything else is read as the old hex digest.
fn secret_format(stored: &str) -> SecretFormat {
    if stored.starts_with("$argon2") {
        SecretFormat::Argon2
    } else {
        SecretFormat::LegacyDigest
    }
}

/// Whether a submitted secret enrolls against a stored credential, and under which hash it did.
/// Both comparisons are constant time: argon2's verifier is, and the legacy digest is compared
/// with `ct_eq` rather than `!=`, which would return at the first differing byte and time the
/// answer.
fn check_credential(
    cred: Option<&credentials_model::Model>,
    client_secret: &str,
) -> Result<SecretFormat, EnrollDenial> {
    use crate::routes::private::api_tokens::service::{hash_token, verify_api_secret};

    let Some(cred) = cred else {
        return Err(EnrollDenial::UnknownClient);
    };
    if cred.revoked {
        return Err(EnrollDenial::Revoked);
    }
    match secret_format(&cred.client_secret_hash) {
        SecretFormat::Argon2 => {
            if verify_api_secret(client_secret, &cred.client_secret_hash) {
                Ok(SecretFormat::Argon2)
            } else {
                Err(EnrollDenial::BadSecret)
            }
        }
        SecretFormat::LegacyDigest => {
            let submitted = hash_token(client_secret);
            if submitted
                .as_bytes()
                .ct_eq(cred.client_secret_hash.as_bytes())
                .into()
            {
                Ok(SecretFormat::LegacyDigest)
            } else {
                Err(EnrollDenial::BadSecret)
            }
        }
    }
}

pub(crate) async fn create_session_token(state: &AppState, service_id: Uuid) -> AppResult<String> {
    let raw_token = super::tokens::generate_token();
    let token_hash = crate::routes::private::api_tokens::service::hash_token(&raw_token);
    let ttl_secs = state.config.sync_session_token_ttl_secs as i64;

    let token = tokens_model::ActiveModel {
        id: Set(Uuid::new_v4()),
        service_id: Set(service_id),
        token_hash: Set(token_hash.clone()),
        expires_at: Set((Utc::now() + chrono::Duration::seconds(ttl_secs)).into()),
        created_at: Set(Utc::now().into()),
    };
    token.insert(&state.db).await?;
    tracing::debug!(%service_id, token_hash_prefix = %&token_hash[..8], "Session token created");

    let db_clone = state.db.clone();
    tokio::spawn(async move {
        let _ = tokens_model::Entity::delete_many()
            .filter(tokens_model::Column::ServiceId.eq(service_id))
            .filter(tokens_model::Column::ExpiresAt.lt(Utc::now()))
            .exec(&db_clone)
            .await;
    });

    Ok(raw_token)
}

/// Enroll a sync service instance with credentials. Validates `client_id`/`client_secret`
/// against `credentials_model`, registers or updates a `services_model` row keyed
/// by `(service_type, instance_id)`, and returns a session token used for subsequent
/// authenticated requests (heartbeat, command updates, events). Unauthenticated.
#[utoipa::path(
    post,
    path = "/api/sync/enroll",
    request_body = EnrollRequest,
    responses(
        (status = 200, description = "Service enrolled; session token returned", body = EnrollResponse),
        (status = 401, description = "Invalid client credentials; the reason is not disclosed"),
    ),
    tag = "sync"
)]
pub async fn enroll(
    State(state): State<AppState>,
    Json(req): Json<EnrollRequest>,
) -> AppResult<Json<EnrollResponse>> {
    let found = credentials_model::Entity::find()
        .filter(credentials_model::Column::ClientId.eq(&req.client_id))
        .one(&state.db)
        .await?;

    let format = match check_credential(found.as_ref(), &req.client_secret) {
        Ok(format) => format,
        Err(denial) => {
            tracing::warn!(
                client_id = %req.client_id,
                reason = denial.reason(),
                "Enrollment refused"
            );
            return Err(AppError::Unauthorized(denial.public_message().to_string()));
        }
    };
    let cred = found.expect("check_credential admits only a credential that was found");

    // The plaintext is held nowhere, so this enrollment is the only moment a credential stored
    // under the old unsalted digest can be re-hashed without re-issuing it.
    if format == SecretFormat::LegacyDigest {
        let mut upgrade: credentials_model::ActiveModel = cred.clone().into();
        upgrade.client_secret_hash = Set(
            crate::routes::private::api_tokens::service::hash_api_secret(&req.client_secret),
        );
        if let Err(e) = upgrade.update(&state.db).await {
            tracing::warn!(client_id = %req.client_id, error = %e, "Credential rehash failed");
        } else {
            tracing::info!(client_id = %req.client_id, "Credential rehashed under argon2id");
        }
    }

    let existing = services_model::Entity::find()
        .filter(
            Condition::all()
                .add(services_model::Column::ServiceType.eq(&cred.service_type))
                .add(services_model::Column::InstanceId.eq(&req.instance_id)),
        )
        .one(&state.db)
        .await?;

    let starting = ServiceStatus::Starting.to_string();

    // `paused` deliberately survives re-enrollment: a pod restart must not
    // undo an operator's pause.
    let (service_id, paused, sync_interval_secs) = if let Some(existing) = existing {
        let mut active: services_model::ActiveModel = existing.clone().into();
        active.status = Set(starting);
        active.current_operation = Set(None);
        active.last_error = Set(None);
        // The credential declares the source system, so a re-declaration reaches the service on
        // its next enrollment rather than waiting for it to be re-created.
        active.source_system = Set(cred.source_system.clone());
        active.updated_at = Set(Utc::now().into());
        active.update(&state.db).await?;
        (existing.id, existing.paused, existing.sync_interval_secs)
    } else {
        let service = services_model::ActiveModel {
            id: Set(Uuid::new_v4()),
            service_type: Set(cred.service_type.clone()),
            source_system: Set(cred.source_system.clone()),
            instance_id: Set(req.instance_id.clone()),
            status: Set(starting),
            paused: Set(false),
            current_operation: Set(None),
            sync_interval_secs: Set(None),
            full_reassert_enabled: Set(false),
            last_heartbeat: Set(None),
            last_sync_completed_at: Set(None),
            last_error: Set(None),
            created_at: Set(Utc::now().into()),
            updated_at: Set(Utc::now().into()),
        };
        let inserted = service.insert(&state.db).await?;
        (inserted.id, false, None)
    };

    if cred.service_id.is_none() {
        let mut cred_active: credentials_model::ActiveModel = cred.into();
        cred_active.service_id = Set(Some(service_id));
        cred_active.update(&state.db).await?;
    }

    let session_token = create_session_token(&state, service_id).await?;
    SESSION_TOKEN_CACHE
        .insert(service_id, session_token.clone())
        .await;

    Ok(Json(EnrollResponse {
        service_id,
        session_token,
        paused,
        sync_interval_secs: sync_interval_secs.map(|s| s as u64),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::private::api_tokens::service::hash_token;

    fn credential(secret: &str, revoked: bool) -> credentials_model::Model {
        credentials_model::Model {
            id: Uuid::nil(),
            client_id: "rvd-sync-1".to_string(),
            client_secret_hash: hash_token(secret),
            service_type: "vaisala".to_string(),
            source_system: Some("vaisala".to_string()),
            service_id: None,
            revoked,
            created_at: Utc::now().into(),
        }
    }

    #[test]
    fn test_check_credential_admits_the_right_secret() {
        let cred = credential("s3cret", false);
        assert_eq!(
            check_credential(Some(&cred), "s3cret"),
            Ok(SecretFormat::LegacyDigest)
        );
    }

    fn argon2_credential(secret: &str) -> credentials_model::Model {
        let mut cred = credential(secret, false);
        cred.client_secret_hash =
            crate::routes::private::api_tokens::service::hash_api_secret(secret);
        cred
    }

    #[test]
    fn test_an_argon2_credential_admits_its_own_secret_and_no_other() {
        let cred = argon2_credential("s3cret");
        assert_eq!(
            check_credential(Some(&cred), "s3cret"),
            Ok(SecretFormat::Argon2)
        );
        assert_eq!(
            check_credential(Some(&cred), "wrong"),
            Err(EnrollDenial::BadSecret)
        );
    }

    // Scenario: the same secret hashed twice.
    // Expected behaviour: argon2 salts, so the two stored hashes differ and a stolen table
    // cannot be attacked with one pass over a wordlist the way the unsalted digest could.
    #[test]
    fn test_two_credentials_sharing_a_secret_store_different_hashes() {
        let a = argon2_credential("s3cret");
        let b = argon2_credential("s3cret");
        assert_ne!(a.client_secret_hash, b.client_secret_hash);
        assert!(a.client_secret_hash.starts_with("$argon2id$"));
    }

    #[test]
    fn test_the_stored_format_decides_which_verifier_runs() {
        assert_eq!(
            secret_format(&crate::routes::private::api_tokens::service::hash_api_secret("s")),
            SecretFormat::Argon2
        );
        assert_eq!(secret_format(&hash_token("s")), SecretFormat::LegacyDigest);
        assert_eq!(secret_format(""), SecretFormat::LegacyDigest);
    }

    // A revoked credential is refused before either verifier runs, whichever format it holds.
    #[test]
    fn test_a_revoked_argon2_credential_is_refused_with_its_own_secret() {
        let mut cred = argon2_credential("s3cret");
        cred.revoked = true;
        assert_eq!(
            check_credential(Some(&cred), "s3cret"),
            Err(EnrollDenial::Revoked)
        );
    }

    // Scenario: a probe walks client ids against /sync/enroll.
    // Expected behaviour: an unknown id, a revoked credential and a wrong secret are refused
    // with one message, so the response says nothing about which ids exist.
    #[test]
    fn test_every_refusal_answers_with_the_same_message() {
        let cred = credential("s3cret", false);
        let revoked = credential("s3cret", true);
        let refusals = [
            check_credential(None, "s3cret"),
            check_credential(Some(&revoked), "s3cret"),
            check_credential(Some(&cred), "wrong"),
        ];
        for r in &refusals {
            let denial = r.as_ref().expect_err("must be refused");
            assert_eq!(denial.public_message(), ENROLL_DENIED);
        }
        // The reason is still distinguishable server-side, for the log.
        assert!(matches!(refusals[0], Err(EnrollDenial::UnknownClient)));
        assert!(matches!(refusals[1], Err(EnrollDenial::Revoked)));
        assert!(matches!(refusals[2], Err(EnrollDenial::BadSecret)));
    }

    #[test]
    fn test_check_credential_rejects_a_secret_whose_hash_is_only_a_prefix() {
        let mut cred = credential("s3cret", false);
        cred.client_secret_hash.truncate(16);
        assert!(matches!(
            check_credential(Some(&cred), "s3cret"),
            Err(EnrollDenial::BadSecret)
        ));
    }

    #[test]
    fn test_check_credential_rejects_an_empty_secret() {
        let cred = credential("s3cret", false);
        assert!(matches!(
            check_credential(Some(&cred), ""),
            Err(EnrollDenial::BadSecret)
        ));
    }

    // A revoked credential is refused even when the secret is right, and the check does not
    // depend on the order the two are tested in.
    #[test]
    fn test_revocation_beats_a_correct_secret() {
        let cred = credential("s3cret", true);
        assert!(matches!(
            check_credential(Some(&cred), "s3cret"),
            Err(EnrollDenial::Revoked)
        ));
    }
}
