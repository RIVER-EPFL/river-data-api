use axum::Json;
use axum::extract::{Path, State};
use chrono::Utc;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use uuid::Uuid;

use super::session::SyncServiceContext;
use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::sync::commands_model;
use river_data_core::models::{CommandStatus, CommandUpdateRequest};

const VALID_UPDATE_STATUSES: &[&str] = &[
    CommandStatus::Acknowledged.as_str(),
    CommandStatus::Completed.as_str(),
    CommandStatus::Failed.as_str(),
];

/// Sync service reports the lifecycle status of a command it received via heartbeat.
/// Valid status transitions: `acknowledged` (in progress), `completed` (success with
/// optional result payload), `failed` (with error result). Only the owning service can
/// update its commands. Requires sync session token auth.
#[utoipa::path(
    patch,
    path = "/commands/{id}",
    params(("id" = Uuid, Path, description = "Sync command UUID")),
    request_body = CommandUpdateRequest,
    responses(
        (status = 200, description = "Command updated"),
        (status = 400, description = "Invalid status value"),
        (status = 401, description = "Invalid session token"),
        (status = 403, description = "Command belongs to a different service"),
        (status = 404, description = "Command not found"),
    ),
    tag = "sync"
)]
pub async fn update_command(
    State(state): State<AppState>,
    ctx: SyncServiceContext,
    Path(command_id): Path<Uuid>,
    Json(req): Json<CommandUpdateRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let cmd = commands_model::Entity::find_by_id(command_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Command not found".to_string()))?;

    if cmd.service_id != ctx.service_id {
        return Err(AppError::Forbidden(
            "Command does not belong to this service".to_string(),
        ));
    }

    if !VALID_UPDATE_STATUSES.contains(&req.status.as_str()) {
        return Err(AppError::BadRequest(format!(
            "Invalid status '{}'. Valid: {}",
            req.status,
            VALID_UPDATE_STATUSES.join(", ")
        )));
    }

    let mut active: commands_model::ActiveModel = cmd.into();
    active.status = Set(req.status.clone());
    if req.result.is_some() {
        active.result = Set(req.result);
    }
    if req.status == CommandStatus::Acknowledged.as_str() {
        active.acknowledged_at = Set(Some(Utc::now().into()));
    }
    if req.status == CommandStatus::Completed.as_str()
        || req.status == CommandStatus::Failed.as_str()
    {
        active.completed_at = Set(Some(Utc::now().into()));
    }
    active.update(&state.db).await?;

    Ok(Json(serde_json::json!({"updated": true})))
}
