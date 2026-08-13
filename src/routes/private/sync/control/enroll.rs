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
    path = "/enroll",
    request_body = EnrollRequest,
    responses(
        (status = 200, description = "Service enrolled; session token returned", body = EnrollResponse),
        (status = 401, description = "Invalid client_id, client_secret, or credentials revoked"),
    ),
    tag = "sync"
)]
pub async fn enroll(
    State(state): State<AppState>,
    Json(req): Json<EnrollRequest>,
) -> AppResult<Json<EnrollResponse>> {
    let cred = credentials_model::Entity::find()
        .filter(credentials_model::Column::ClientId.eq(&req.client_id))
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::Unauthorized("Invalid client_id".to_string()))?;

    if cred.revoked {
        return Err(AppError::Unauthorized(
            "Credentials have been revoked".to_string(),
        ));
    }

    let secret_hash = crate::routes::private::api_tokens::service::hash_token(&req.client_secret);
    if secret_hash != cred.client_secret_hash {
        return Err(AppError::Unauthorized("Invalid client_secret".to_string()));
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
    let (service_id, paused) = if let Some(existing) = existing {
        let mut active: services_model::ActiveModel = existing.clone().into();
        active.status = Set(starting);
        active.current_operation = Set(None);
        active.last_error = Set(None);
        active.updated_at = Set(Utc::now().into());
        active.update(&state.db).await?;
        (existing.id, existing.paused)
    } else {
        let service = services_model::ActiveModel {
            id: Set(Uuid::new_v4()),
            service_type: Set(cred.service_type.clone()),
            instance_id: Set(req.instance_id.clone()),
            status: Set(starting),
            paused: Set(false),
            current_operation: Set(None),
            last_heartbeat: Set(None),
            last_sync_completed_at: Set(None),
            last_error: Set(None),
            created_at: Set(Utc::now().into()),
            updated_at: Set(Utc::now().into()),
        };
        let inserted = service.insert(&state.db).await?;
        (inserted.id, false)
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
    }))
}
