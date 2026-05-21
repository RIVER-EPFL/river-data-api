use axum::{
    Json,
    extract::{Path, Query, State},
    http::header::{self, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::ReceiverStream;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::error::{AppError, AppResult};
use crate::routes::{resolve_site, validate_optional_time_range};
use crate::common::bulk;

use super::types::SiteRef;

/// A single status event row from the database
#[derive(Debug, FromQueryResult)]
struct StatusEventRow {
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::FixedOffset>,
    value: String,
    sensor_id: Option<Uuid>,
}

fn default_format() -> String {
    "json".to_string()
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct StatusEventsQuery {
    /// Start time (optional, ISO 8601). If omitted, returns from earliest data.
    pub start: Option<DateTime<Utc>>,
    /// End time (optional, ISO 8601). If omitted, returns to latest data.
    pub end: Option<DateTime<Utc>>,
    /// Response format: json (default), ndjson, csv
    #[serde(default = "default_format")]
    pub format: String,
}

/// A single status event in the JSON response
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StatusEventData {
    pub parameter_id: Uuid,
    pub time: DateTime<Utc>,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sensor_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StatusEventsResponse {
    /// Site this data belongs to
    pub site: SiteRef,
    /// Array of status events
    pub events: Vec<StatusEventData>,
}

/// Get status events for a specific site
///
/// Returns non-numeric time-series events (device status strings, firmware versions)
/// for the specified site. Supports JSON, CSV, and NDJSON formats.
#[utoipa::path(
    get,
    path = "/{site_id}/status_events",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
        StatusEventsQuery
    ),
    responses(
        (status = 200, description = "Status events retrieved successfully", body = StatusEventsResponse),
        (status = 400, description = "Invalid query parameters"),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn get_site_status_events(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
    Query(query): Query<StatusEventsQuery>,
    ProjectScope(scope): ProjectScope,
    headers: HeaderMap,
) -> AppResult<Response> {
    let site = resolve_site(&state.db, &site_id).await?;

    // Enforce project scope
    if let Some(scope_project) = scope
        && site.project_id != Some(scope_project)
    {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }

    let site_ref = SiteRef {
        id: site.id,
        name: site.name.clone(),
    };

    validate_optional_time_range(query.start, query.end)?;

    // Determine format from query or Accept header
    let format = bulk::determine_format(&query.format, &headers);

    let _permit = bulk::acquire_bulk_permit(&format, &state.bulk_semaphore)?;

    // Build parameterized raw SQL query
    let mut values: Vec<sea_orm::Value> = vec![site.id.into()];
    let mut next_param = 2;

    let time_conditions = match (query.start, query.end) {
        (Some(start), Some(end)) => {
            let cond = format!(
                " AND time >= ${} AND time <= ${}",
                next_param,
                next_param + 1
            );
            values.push(start.into());
            values.push(end.into());
            next_param += 2;
            cond
        }
        (Some(start), None) => {
            let cond = format!(" AND time >= ${next_param}");
            values.push(start.into());
            next_param += 1;
            cond
        }
        (None, Some(end)) => {
            let cond = format!(" AND time <= ${next_param}");
            values.push(end.into());
            next_param += 1;
            cond
        }
        (None, None) => String::new(),
    };
    let _ = next_param; // suppress unused warning

    let sql = format!(
        "SELECT parameter_id, time, value, sensor_id FROM status_events WHERE site_id = $1{time_conditions} ORDER BY time"
    );

    let query_result = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?;

    let events: Vec<StatusEventData> = query_result
        .iter()
        .filter_map(|row| {
            StatusEventRow::from_query_result(row, "").ok().map(|r| StatusEventData {
                parameter_id: r.parameter_id,
                time: r.time.with_timezone(&Utc),
                value: r.value,
                sensor_id: r.sensor_id,
            })
        })
        .collect();

    match format.as_str() {
        "csv" => build_status_events_csv(&events),
        "ndjson" => build_status_events_ndjson(&events),
        _ => {
            let response = StatusEventsResponse {
                site: site_ref,
                events,
            };
            Ok(Json(response).into_response())
        }
    }
}

/// Build a streaming CSV response for status events.
fn build_status_events_csv(events: &[StatusEventData]) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let events: Vec<StatusEventData> = events.to_vec();

    tokio::spawn(async move {
        let _ = tx.send(Ok("time,parameter_id,value,sensor_id\n".to_string())).await;

        for event in &events {
            let sensor_id_str = event
                .sensor_id
                .map(|id| id.to_string())
                .unwrap_or_default();
            let escaped_value = format!("\"{}\"", event.value.replace('"', "\"\""));
            let row = format!(
                "{},{},{},{}\n",
                event.time.to_rfc3339(),
                event.parameter_id,
                escaped_value,
                sensor_id_str
            );
            if tx.send(Ok(row)).await.is_err() {
                break;
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    let body = axum::body::Body::from_stream(stream);

    Response::builder()
        .header(header::CONTENT_TYPE, HeaderValue::from_static("text/csv"))
        .body(body)
        .map_err(|e| AppError::Internal(e.to_string()))
}

/// Build a streaming NDJSON response for status events.
fn build_status_events_ndjson(events: &[StatusEventData]) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let events: Vec<StatusEventData> = events.to_vec();

    tokio::spawn(async move {
        for event in &events {
            let line = format!("{}\n", serde_json::to_string(event).unwrap_or_default());
            if tx.send(Ok(line)).await.is_err() {
                break;
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    let body = axum::body::Body::from_stream(stream);

    Response::builder()
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-ndjson"),
        )
        .body(body)
        .map_err(|e| AppError::Internal(e.to_string()))
}
