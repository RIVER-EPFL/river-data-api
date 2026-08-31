use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, Statement};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::error::{AppError, AppResult};
use crate::routes::private::{annotations, parameters};
use crate::routes::{resolve_site, validate_optional_time_range};

#[derive(Debug, Deserialize, IntoParams)]
pub struct SiteAnnotationsQuery {
    /// Filter by parameter UUID
    pub parameter_id: Option<Uuid>,
    /// Filter by a comma-separated list of parameter UUIDs
    pub parameter_ids: Option<String>,
    /// Start time (ISO 8601)
    pub start: Option<DateTime<Utc>>,
    /// End time (ISO 8601)
    pub end: Option<DateTime<Utc>>,
    /// Response format: json (default) or csv
    pub format: Option<String>,
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
    /// The sync source that registered the annotation; NULL on hand-entered ones.
    pub source_system: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
}

/// A CSV field with commas, quotes and newlines escaped.
fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
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
) -> AppResult<Response> {
    let site = resolve_site(&state.db, &site_id).await?;

    // Enforce project scope
    if !scope.allows_project_opt(site.project_id) {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }

    validate_optional_time_range(query.start, query.end)?;

    let mut q = annotations::Entity::find().filter(annotations::Column::SiteId.eq(site.id));

    if let Some(param_id) = query.parameter_id {
        q = q.filter(annotations::Column::ParameterId.eq(param_id));
    }
    if let Some(ids) = &query.parameter_ids {
        let ids: Vec<Uuid> = ids
            .split(',')
            .map(|s| {
                s.trim()
                    .parse()
                    .map_err(|_| AppError::BadRequest(format!("Invalid parameter id '{s}'")))
            })
            .collect::<Result<_, _>>()?;
        q = q.filter(annotations::Column::ParameterId.is_in(ids));
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

    if query.format.as_deref() == Some("csv") {
        let mut param_ids: Vec<Uuid> = rows.iter().map(|a| a.parameter_id).collect();
        param_ids.sort_unstable();
        param_ids.dedup();
        let codes: HashMap<Uuid, String> = parameters::Entity::find()
            .filter(parameters::Column::Id.is_in(param_ids))
            .all(&state.db)
            .await?
            .into_iter()
            .map(|p| (p.id, p.code))
            .collect();

        let mut csv = String::from(
            "site,parameter_code,category,start_time,end_time,text,created_by,source_system\n",
        );
        for a in &rows {
            csv.push_str(&format!(
                "{},{},{},{},{},{},{},{}\n",
                csv_field(&site.name),
                csv_field(codes.get(&a.parameter_id).map(String::as_str).unwrap_or("")),
                csv_field(&a.category),
                a.start_time.to_rfc3339(),
                a.end_time.to_rfc3339(),
                csv_field(&a.text),
                csv_field(a.created_by.as_deref().unwrap_or("")),
                csv_field(a.source_system.as_deref().unwrap_or("")),
            ));
        }
        return Response::builder()
            .header(header::CONTENT_TYPE, HeaderValue::from_static("text/csv"))
            .body(axum::body::Body::from(csv))
            .map_err(|e| AppError::Internal(e.to_string()));
    }

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
            source_system: a.source_system,
            created_at: a.created_at,
        })
        .collect();

    Ok(Json(response).into_response())
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct ExportSummaryQuery {
    /// Start time (ISO 8601)
    pub start: DateTime<Utc>,
    /// End time (ISO 8601)
    pub end: DateTime<Utc>,
}

#[derive(Debug, Default, Serialize, ToSchema)]
pub struct ParameterExportSummary {
    pub parameter_id: Uuid,
    pub code: String,
    pub annotation_count: i64,
    /// Distinct served instants of the parameter inside both the query range and an
    /// annotation's own range.
    pub annotated_points: i64,
    pub flagged_readings: i64,
    /// Served readings beyond the first replicate slot (replicate_index > 0).
    pub replicate_readings: i64,
    /// Readings breaching a warning or alarm bound over the range.
    pub alarm_readings: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExportSummaryResponse {
    pub annotation_count: i64,
    pub annotated_points: i64,
    pub flagged_readings: i64,
    pub replicate_readings: i64,
    pub alarm_readings: i64,
    pub per_parameter: Vec<ParameterExportSummary>,
}

/// What an export of this site and range can carry beyond the plain series: annotation, flagged,
/// replicate and alarm counts, per parameter. The export dialog enables each option from these
/// numbers and shows them beside it.
#[utoipa::path(
    get,
    path = "/{site_id}/export/summary",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
        ExportSummaryQuery
    ),
    responses(
        (status = 200, body = ExportSummaryResponse),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn get_site_export_summary(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
    Query(query): Query<ExportSummaryQuery>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<ExportSummaryResponse>> {
    let site = resolve_site(&state.db, &site_id).await?;
    if !scope.allows_project_opt(site.project_id) {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }
    if query.end <= query.start {
        return Err(AppError::BadRequest("end must be after start".to_string()));
    }
    let range: [sea_orm::Value; 3] = [
        site.id.into(),
        sea_orm::prelude::DateTimeWithTimeZone::from(query.start).into(),
        sea_orm::prelude::DateTimeWithTimeZone::from(query.end).into(),
    ];

    let mut by_param: std::collections::BTreeMap<Uuid, ParameterExportSummary> =
        std::collections::BTreeMap::new();
    fn slot(
        map: &mut std::collections::BTreeMap<Uuid, ParameterExportSummary>,
        id: Uuid,
    ) -> &mut ParameterExportSummary {
        map.entry(id).or_insert_with(|| ParameterExportSummary {
            parameter_id: id,
            ..Default::default()
        })
    }

    // Annotations overlapping the range, each joined to the served (non-withdrawn) readings its
    // own window covers, clipped to the query range. DISTINCT r.time so two overlapping
    // annotations do not double-count an instant within a parameter.
    let rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT a.parameter_id AS pid,
                    COUNT(DISTINCT a.id) AS ann_count,
                    COUNT(DISTINCT r.time) AS pts
             FROM annotations a
             LEFT JOIN readings r
               ON r.site_id = a.site_id AND r.parameter_id = a.parameter_id
              AND r.time >= GREATEST(a.start_time, $2) AND r.time <= LEAST(a.end_time, $3)
              AND r.withdrawn_at IS NULL
             WHERE a.site_id = $1 AND a.end_time >= $2 AND a.start_time <= $3
             GROUP BY a.parameter_id",
            range.clone(),
        ))
        .await?;
    for r in &rows {
        let s = slot(&mut by_param, r.try_get::<Uuid>("", "pid")?);
        s.annotation_count = r.try_get::<i64>("", "ann_count")?;
        s.annotated_points = r.try_get::<i64>("", "pts")?;
    }

    // Flagged and extra-replicate rows in one pass over the range's readings.
    let rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT parameter_id AS pid,
                    COUNT(*) FILTER (WHERE is_flagged = TRUE) AS flagged,
                    COUNT(*) FILTER (WHERE replicate_index > 0) AS reps
             FROM readings
             WHERE site_id = $1 AND time >= $2 AND time <= $3 AND withdrawn_at IS NULL
               AND (is_flagged = TRUE OR replicate_index > 0)
             GROUP BY parameter_id",
            range,
        ))
        .await?;
    for r in &rows {
        let s = slot(&mut by_param, r.try_get::<Uuid>("", "pid")?);
        s.flagged_readings = r.try_get::<i64>("", "flagged")?;
        s.replicate_readings = r.try_get::<i64>("", "reps")?;
    }

    // Breaching readings, from the same definition `/sites/{id}/alarms` serves, so the count and
    // the export it gates cannot disagree. `alarm_events` is deliberately not the source: it holds
    // the sweeper's episodes, which exist only from when the sweeper first saw a slot, while an
    // export covers all of history.
    for (pid, n) in crate::routes::private::alarms::views::count_violations_by_parameter(
        &state.db, site.id, query.start, query.end,
    )
    .await?
    {
        slot(&mut by_param, pid).alarm_readings = n;
    }

    let ids: Vec<Uuid> = by_param.keys().copied().collect();
    let codes: HashMap<Uuid, String> = parameters::Entity::find()
        .filter(parameters::Column::Id.is_in(ids))
        .all(&state.db)
        .await?
        .into_iter()
        .map(|p| (p.id, p.code))
        .collect();
    let mut per_parameter: Vec<ParameterExportSummary> = by_param
        .into_values()
        .map(|mut s| {
            s.code = codes.get(&s.parameter_id).cloned().unwrap_or_default();
            s
        })
        .collect();
    per_parameter.sort_by(|a, b| a.code.cmp(&b.code));

    Ok(Json(ExportSummaryResponse {
        annotation_count: per_parameter.iter().map(|p| p.annotation_count).sum(),
        annotated_points: per_parameter.iter().map(|p| p.annotated_points).sum(),
        flagged_readings: per_parameter.iter().map(|p| p.flagged_readings).sum(),
        replicate_readings: per_parameter.iter().map(|p| p.replicate_readings).sum(),
        alarm_readings: per_parameter.iter().map(|p| p.alarm_readings).sum(),
        per_parameter,
    }))
}
