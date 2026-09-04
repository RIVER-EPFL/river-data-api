//! The replicate rows behind a site's spot instants, as their own download.
//!
//! The site export is a row per instant carrying the statistics; this is a row per replicate,
//! joined to it by `sample_id`. Keeping them apart is what lets one file hold the statistics and
//! the other the measurements they were computed from, rather than a shape that is neither.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::header::{self, HeaderValue},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, FixedOffset, Utc};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::error::{AppError, AppResult};
use crate::routes::{resolve_site_with_project, validate_optional_time_range};

use super::types::SiteRef;

#[derive(Debug, Deserialize, IntoParams)]
pub struct ReplicatesQuery {
    /// Start of the range (optional, ISO 8601). Defaults to the configured lookback.
    pub start: Option<DateTime<Utc>>,
    /// End of the range (optional, ISO 8601). Open-ended when omitted.
    pub end: Option<DateTime<Utc>>,
    /// Restrict to these global parameter ids (comma-separated).
    pub parameter_ids: Option<String>,
    /// Include the replicates of instants the source has retracted, marked as retracted.
    pub include_withdrawn: Option<bool>,
    /// Response format: json (default) or csv.
    #[serde(default = "crate::common::bulk::default_format")]
    pub format: String,
}

/// One measurement behind a spot instant.
#[derive(Debug, Serialize, ToSchema, FromQueryResult)]
pub struct ReplicateRow {
    pub time: DateTime<FixedOffset>,
    /// The catalog code, which is the column name the site export writes the instant under.
    pub parameter: String,
    /// The instant's sample, `{code}_sample_id` on the site export. Null where the instant holds
    /// one measurement, which forms no sample.
    pub sample_id: Option<Uuid>,
    pub replicate_index: i16,
    /// Corrected where a curve applied, raw otherwise: the value the statistics were computed from.
    pub value: Option<f64>,
    pub flagged: bool,
    pub withdrawn: bool,
    pub source_system: Option<String>,
    pub source_key: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReplicatesResponse {
    pub site: SiteRef,
    pub rows: Vec<ReplicateRow>,
}

/// Every replicate of every spot instant at a site, one row each.
///
/// Joins to the site export on `sample_id`, with `(parameter, time)` as the fallback for an
/// instant that holds a single measurement. Supports JSON (default) and CSV.
#[utoipa::path(
    get,
    path = "/api/sites/{site_id}/export/replicates",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
        ReplicatesQuery
    ),
    responses(
        (status = 200, description = "Replicate rows", body = ReplicatesResponse),
        (status = 400, description = "Invalid query parameters"),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn get_site_replicates(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
    Query(query): Query<ReplicatesQuery>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Response> {
    let (site, _project) = resolve_site_with_project(&state.db, &site_id).await?;
    if !scope.allows_project_opt(site.project_id) {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }

    let effective_start = query.start.unwrap_or_else(|| {
        Utc::now() - chrono::Duration::days(state.config.default_readings_lookback_days)
    });
    validate_optional_time_range(Some(effective_start), query.end)?;

    let parameter_ids = match query.parameter_ids.as_deref() {
        Some(list) => {
            let parsed: Vec<Uuid> = list
                .split(',')
                .filter_map(|s| Uuid::parse_str(s.trim()).ok())
                .collect();
            if parsed.is_empty() {
                return Err(AppError::BadRequest(
                    "parameter_ids was provided but no UUIDs could be parsed".to_string(),
                ));
            }
            Some(parsed)
        }
        None => None,
    };

    let mut values: Vec<sea_orm::Value> = vec![site.id.into(), effective_start.into()];
    let mut conditions = String::new();
    if let Some(end) = query.end {
        values.push(end.into());
        conditions.push_str(&format!(" AND r.time <= ${}", values.len()));
    }
    if let Some(ids) = parameter_ids {
        values.push(ids.into());
        conditions.push_str(&format!(" AND r.parameter_id = ANY(${})", values.len()));
    }
    if !query.include_withdrawn.unwrap_or(false) {
        conditions.push_str(" AND r.withdrawn_at IS NULL");
    }

    let sql = format!(
        r"SELECT r.time,
                 p.code AS parameter,
                 r.sample_id,
                 r.replicate_index,
                 COALESCE(r.calibrated_value, r.raw_value) AS value,
                 COALESCE(r.is_flagged, false) AS flagged,
                 (r.withdrawn_at IS NOT NULL) AS withdrawn,
                 ds.source_system,
                 ds.source_key
          FROM readings r
          JOIN parameters p ON p.id = r.parameter_id
          LEFT JOIN data_streams ds ON ds.id = r.stream_id
          WHERE r.site_id = $1 AND r.time >= $2
            AND r.measurement_type = 'spot'
            AND r.sample_id IS NOT NULL{conditions}
          ORDER BY r.time, p.code, r.replicate_index"
    );

    let rows: Vec<ReplicateRow> = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?
        .iter()
        .filter_map(|row| ReplicateRow::from_query_result(row, "").ok())
        .collect();

    if query.format == "csv" {
        let mut csv = String::from(
            "time,parameter,sample_id,replicate_index,value,flagged,withdrawn,source_system,source_key\n",
        );
        for r in &rows {
            csv.push_str(&format!(
                "{},{},{},{},{},{},{},{},{}\n",
                r.time.with_timezone(&Utc).to_rfc3339(),
                r.parameter,
                r.sample_id.map(|id| id.to_string()).unwrap_or_default(),
                r.replicate_index,
                r.value.map(|v| v.to_string()).unwrap_or_default(),
                r.flagged,
                r.withdrawn,
                r.source_system.clone().unwrap_or_default(),
                r.source_key.clone().unwrap_or_default(),
            ));
        }
        return Response::builder()
            .header(header::CONTENT_TYPE, HeaderValue::from_static("text/csv"))
            .body(axum::body::Body::from(csv))
            .map_err(|e| AppError::Internal(e.to_string()));
    }

    Ok(Json(ReplicatesResponse {
        site: SiteRef {
            id: site.id,
            name: site.name.clone(),
        },
        rows,
    })
    .into_response())
}
