use axum::extract::State;
use axum::Json;
use chrono::Utc;
use moka::future::Cache;
use sea_orm::{ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, EntityTrait, QueryFilter, Set, Statement, DatabaseBackend};
use std::sync::LazyLock;
use std::time::Duration;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};
use river_data_core::models::{CommandStatus, HeartbeatRequest, HeartbeatResponse, PendingCommand, ServiceStatus};
use crate::routes::private::sync::{commands_model, services_model};
use super::enroll::create_session_token;
use super::session::SyncServiceContext;


pub(crate) static SESSION_TOKEN_CACHE: LazyLock<Cache<Uuid, String>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(100)
        .time_to_live(Duration::from_secs(13 * 60))
        .build()
});

/// Periodic heartbeat from a sync service. Updates `last_heartbeat`, `status`, and
/// `current_operation`. Returns a fresh session token (rotated on every heartbeat) and
/// any pending commands queued for this service. Requires sync session token auth.
#[utoipa::path(
    post,
    path = "/heartbeat",
    request_body = HeartbeatRequest,
    responses(
        (status = 200, description = "Heartbeat acknowledged; fresh token and pending commands", body = HeartbeatResponse),
        (status = 400, description = "Invalid status string"),
        (status = 401, description = "Invalid or expired session token"),
        (status = 403, description = "service_id does not match the authenticated service"),
    ),
    tag = "sync"
)]
pub async fn heartbeat(
    State(state): State<AppState>,
    ctx: SyncServiceContext,
    Json(req): Json<HeartbeatRequest>,
) -> AppResult<Json<HeartbeatResponse>> {
    if req.service_id != ctx.service_id {
        return Err(AppError::Forbidden(
            "Heartbeat service_id does not match the authenticated service".to_string(),
        ));
    }

    if ServiceStatus::from_str(&req.status).is_none() {
        let valid: Vec<&str> = ServiceStatus::ALL.iter().map(|s| s.as_str()).collect();
        return Err(AppError::BadRequest(format!(
            "Invalid status '{}'. Valid: {}",
            req.status,
            valid.join(", ")
        )));
    }

    let service = services_model::Entity::find_by_id(req.service_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Service not found".to_string()))?;

    let paused = service.paused;
    let mut active: services_model::ActiveModel = service.into();
    active.status = Set(req.status);
    active.current_operation = Set(req.current_operation);
    active.last_heartbeat = Set(Some(Utc::now().into()));
    active.updated_at = Set(Utc::now().into());
    active.update(&state.db).await?;

    let session_token = if let Some(cached) = SESSION_TOKEN_CACHE.get(&req.service_id).await {
        cached
    } else {
        let token = create_session_token(&state, req.service_id).await?;
        SESSION_TOKEN_CACHE
            .insert(req.service_id, token.clone())
            .await;
        token
    };

    let pending = commands_model::Entity::find()
        .filter(
            Condition::all()
                .add(commands_model::Column::ServiceId.eq(req.service_id))
                .add(commands_model::Column::Status.eq(CommandStatus::Pending.as_str()))
                .add(commands_model::Column::ExpiresAt.gt(Utc::now())),
        )
        .all(&state.db)
        .await?;

    let pending_commands = pending
        .into_iter()
        .map(|c| PendingCommand {
            id: c.id,
            command: c.command,
            payload: c.payload,
        })
        .collect();

    let db_clone = state.db.clone();
    let sid = req.service_id;
    tokio::spawn(async move {
        let _ = db_clone
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "UPDATE sync_commands SET status = $2 WHERE service_id = $1 AND status = $3 AND expires_at < NOW()",
                [
                    sid.into(),
                    CommandStatus::Expired.as_str().into(),
                    CommandStatus::Pending.as_str().into(),
                ],
            ))
            .await;
    });

    Ok(Json(HeartbeatResponse {
        session_token,
        pending_commands,
        paused,
    }))
}
