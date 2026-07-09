use axum::{
    Json,
    extract::{Path, Query, State},
};
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::routes::private::annotations;
use crate::error::{AppError, AppResult};
use crate::routes::{resolve_site, validate_optional_time_range};

#[derive(Debug, Deserialize, IntoParams)]
pub struct SiteAnnotationsQuery {
    /// Filter by parameter UUID
    pub parameter_id: Option<Uuid>,
    /// Start time (ISO 8601)
    pub start: Option<DateTime<Utc>>,
    /// End time (ISO 8601)
    pub end: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AnnotationResponse {
    pub id: Uuid,
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub text: String,
    pub category: String,
    pub created_by: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
}

/// List annotations for a site, optionally filtered by parameter and time range
#[utoipa::path(
    get,
    path = "/{site_id}/annotations",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
        SiteAnnotationsQuery
    ),
    responses(
        (status = 200, description = "Annotations retrieved successfully", body = Vec<AnnotationResponse>),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn get_site_annotations(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
    Query(query): Query<SiteAnnotationsQuery>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<Vec<AnnotationResponse>>> {
    let site = resolve_site(&state.db, &site_id).await?;

    // Enforce project scope
    if !scope.allows_project_opt(site.project_id) {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }

    validate_optional_time_range(query.start, query.end)?;

    let mut q = annotations::Entity::find()
        .filter(annotations::Column::SiteId.eq(site.id));

    if let Some(param_id) = query.parameter_id {
        q = q.filter(annotations::Column::ParameterId.eq(param_id));
    }
    if let Some(start) = query.start {
        // Include annotations that overlap with the query range
        q = q.filter(annotations::Column::EndTime.gte(start));
    }
    if let Some(end) = query.end {
        q = q.filter(annotations::Column::StartTime.lte(end));
    }

    let rows = q
        .order_by_asc(annotations::Column::StartTime)
        .all(&state.db)
        .await?;

    let response: Vec<AnnotationResponse> = rows
        .into_iter()
        .map(|a| AnnotationResponse {
            id: a.id,
            site_id: a.site_id,
            parameter_id: a.parameter_id,
            start_time: a.start_time,
            end_time: a.end_time,
            text: a.text,
            category: a.category,
            created_by: a.created_by,
            created_at: a.created_at,
        })
        .collect();

    Ok(Json(response))
}
