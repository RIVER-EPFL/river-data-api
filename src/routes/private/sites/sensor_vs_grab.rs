//! Sensor-vs-grab comparison export (ported from the CNET/METALP RShiny portals'
//! `sensor_grab_comparison` + `sensor_vs_grab_download`). For each grab sample at time T, it averages
//! the continuous sensor readings in the window [T + window_start_hours, T + window_end_hours] and
//! pairs them: {grab_value, sensor_avg, sensor_sd, n, difference}. Backs an overlay/1:1-scatter view
//! and a paired CSV download. Unlike the portals (separate grab/sensor tables joined by a
//! `grab_param_name` map), here grabs and continuous readings share `site_id`+`parameter_id` and
//! differ only by `measurement_type`, so the pairing is a single self-join.

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

fn default_window_start() -> f64 {
    2.0
}
fn default_window_end() -> f64 {
    6.0
}
#[derive(Debug, Deserialize, IntoParams)]
pub struct SensorVsGrabQuery {
    /// Global parameter id to compare (continuous sensor readings vs grab samples).
    pub parameter_id: Uuid,
    /// Start of the grab-sample time range (optional, ISO 8601). Defaults to the configured lookback.
    pub start: Option<DateTime<Utc>>,
    /// End of the grab-sample time range (optional, ISO 8601). Open-ended when omitted.
    pub end: Option<DateTime<Utc>>,
    /// Start of the post-grab averaging window, in hours after each grab (default 2).
    #[serde(default = "default_window_start")]
    pub window_start_hours: f64,
    /// End of the post-grab averaging window, in hours after each grab (default 6).
    #[serde(default = "default_window_end")]
    pub window_end_hours: f64,
    /// Response format: json (default) or csv.
    #[serde(default = "crate::common::bulk::default_format")]
    pub format: String,
}

/// One grab sample paired with the continuous-sensor average over the post-grab window.
#[derive(Debug, Serialize, ToSchema)]
pub struct SensorVsGrabRow {
    /// Grab sample collection time.
    pub time: DateTime<Utc>,
    /// Grab sample value (mean of replicates).
    pub grab_value: Option<f64>,
    /// Grab sample standard deviation across replicates.
    pub grab_sd: Option<f64>,
    /// Number of grab replicates.
    pub grab_n: i32,
    /// Mean continuous sensor reading over [time + window_start_hours, time + window_end_hours].
    pub sensor_avg: Option<f64>,
    /// Standard deviation of continuous sensor readings in the window.
    pub sensor_sd: Option<f64>,
    /// Number of continuous sensor readings in the window.
    pub sensor_n: i64,
    /// grab_value − sensor_avg (null when either side is missing).
    pub difference: Option<f64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SensorVsGrabResponse {
    pub site: SiteRef,
    pub parameter_id: Uuid,
    pub window_start_hours: f64,
    pub window_end_hours: f64,
    pub rows: Vec<SensorVsGrabRow>,
}

#[derive(Debug, FromQueryResult)]
struct ComparisonRow {
    grab_time: DateTime<FixedOffset>,
    grab_value: Option<f64>,
    grab_sd: Option<f64>,
    grab_n: i32,
    sensor_avg: Option<f64>,
    sensor_sd: Option<f64>,
    sensor_n: i64,
}

/// Sensor-vs-grab comparison for one parameter at a site.
///
/// Time-aligns each grab sample to the continuous sensor readings in a window after it and returns
/// the paired values plus their difference. Supports JSON (default) and CSV.
#[utoipa::path(
    get,
    path = "/{site_id}/export/sensor-vs-grab",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
        SensorVsGrabQuery
    ),
    responses(
        (status = 200, description = "Sensor-vs-grab comparison", body = SensorVsGrabResponse),
        (status = 400, description = "Invalid query parameters"),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn get_sensor_vs_grab(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
    Query(query): Query<SensorVsGrabQuery>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Response> {
    let (site, _project) = resolve_site_with_project(&state.db, &site_id).await?;

    if !scope.allows_project_opt(site.project_id) {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }

    if query.window_end_hours <= query.window_start_hours {
        return Err(AppError::BadRequest(
            "window_end_hours must be greater than window_start_hours".to_string(),
        ));
    }

    let effective_start = query.start.unwrap_or_else(|| {
        Utc::now() - chrono::Duration::days(state.config.default_readings_lookback_days)
    });
    let effective_end = query.end;
    validate_optional_time_range(Some(effective_start), effective_end)?;

    let mut values: Vec<sea_orm::Value> = vec![
        site.id.into(),
        query.parameter_id.into(),
        effective_start.into(),
        query.window_start_hours.into(),
        query.window_end_hours.into(),
    ];
    let end_condition = match effective_end {
        Some(end) => {
            values.push(end.into());
            " AND s.collected_at <= $6"
        }
        None => "",
    };

    // Continuous = anything that is not a grab ('spot') or derived reading; `IS DISTINCT FROM`
    // keeps NULL-typed legacy/seed readings on the continuous side.
    let sql = format!(
        r#"
        SELECT
            s.collected_at AS grab_time,
            s.mean         AS grab_value,
            s.stdev        AS grab_sd,
            s.n            AS grab_n,
            agg.sensor_avg,
            agg.sensor_sd,
            agg.sensor_n
        FROM samples s
        LEFT JOIN LATERAL (
            SELECT
                avg(COALESCE(r.calibrated_value, r.raw_value))         AS sensor_avg,
                stddev_samp(COALESCE(r.calibrated_value, r.raw_value)) AS sensor_sd,
                count(*)                                               AS sensor_n
            FROM readings r
            WHERE r.site_id = s.site_id
              AND r.parameter_id = s.parameter_id
              AND r.measurement_type IS DISTINCT FROM 'spot'
              AND r.measurement_type IS DISTINCT FROM 'derived'
              AND r.is_flagged IS NOT TRUE
              AND r.replicate_index = 0
              AND r.time >= s.collected_at + ($4 * interval '1 hour')
              AND r.time <= s.collected_at + ($5 * interval '1 hour')
        ) agg ON true
        WHERE s.site_id = $1 AND s.parameter_id = $2 AND s.collected_at >= $3{end_condition}
        ORDER BY s.collected_at
        "#,
    );

    let query_result = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?;

    let rows: Vec<SensorVsGrabRow> = query_result
        .iter()
        .filter_map(|row| ComparisonRow::from_query_result(row, "").ok())
        .map(|c| {
            let difference = match (c.grab_value, c.sensor_avg) {
                (Some(g), Some(s)) => Some(g - s),
                _ => None,
            };
            SensorVsGrabRow {
                time: c.grab_time.with_timezone(&Utc),
                grab_value: c.grab_value,
                grab_sd: c.grab_sd,
                grab_n: c.grab_n,
                sensor_avg: c.sensor_avg,
                sensor_sd: c.sensor_sd,
                sensor_n: c.sensor_n,
                difference,
            }
        })
        .collect();

    if query.format == "csv" {
        let fmt = |v: Option<f64>| v.map(|x| x.to_string()).unwrap_or_default();
        let mut csv = String::from(
            "time,grab_value,grab_sd,grab_n,sensor_avg,sensor_sd,sensor_n,difference\n",
        );
        for r in &rows {
            csv.push_str(&format!(
                "{},{},{},{},{},{},{},{}\n",
                r.time.to_rfc3339(),
                fmt(r.grab_value),
                fmt(r.grab_sd),
                r.grab_n,
                fmt(r.sensor_avg),
                fmt(r.sensor_sd),
                r.sensor_n,
                fmt(r.difference),
            ));
        }
        return Response::builder()
            .header(header::CONTENT_TYPE, HeaderValue::from_static("text/csv"))
            .body(axum::body::Body::from(csv))
            .map_err(|e| AppError::Internal(e.to_string()));
    }

    Ok(Json(SensorVsGrabResponse {
        site: SiteRef {
            id: site.id,
            name: site.name.clone(),
        },
        parameter_id: query.parameter_id,
        window_start_hours: query.window_start_hours,
        window_end_hours: query.window_end_hours,
        rows,
    })
    .into_response())
}
