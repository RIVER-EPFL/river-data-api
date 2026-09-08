use axum::Json;
use axum::extract::State;
use sea_orm::{ConnectionTrait, Statement};
use serde::Serialize;
use utoipa::ToSchema;

use crate::common::AppState;
use crate::error::AppResult;

/// Distinct HTTP status codes present in the audit log, used to populate the admin filter dropdown
/// with the values that actually occur rather than a free-form number box.
#[derive(Debug, Serialize, ToSchema)]
pub struct AuditStatusCodes {
    pub status_codes: Vec<i32>,
}

/// Distinct `status_code` values recorded in `api_token_audit_log`, ascending. Admin-only, read-only.
#[utoipa::path(
    get,
    path = "/api/api_token_audit_logs/distinct/status_codes",
    responses(
        (status = 200, description = "Distinct status codes recorded in the audit log", body = AuditStatusCodes),
    ),
    tag = "tokens"
)]
pub async fn distinct_status_codes(
    State(state): State<AppState>,
) -> AppResult<Json<AuditStatusCodes>> {
    let rows = state
        .db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT DISTINCT status_code FROM api_token_audit_log ORDER BY status_code".to_string(),
        ))
        .await?;

    let status_codes = rows
        .iter()
        .map(|r| Ok(r.try_get::<i32>("", "status_code")?))
        .collect::<AppResult<Vec<i32>>>()?;

    Ok(Json(AuditStatusCodes { status_codes }))
}
