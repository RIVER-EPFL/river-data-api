use axum::{
    Json,
    extract::{Path, Query, State},
    http::header::HeaderMap,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, Statement};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::aggregates::Resolution;
use crate::common::middleware::ProjectScope;
use crate::common::series::{self, Cells, Table};
use crate::common::{bulk, cache_key};
use crate::error::{AppError, AppResult};
use crate::routes::private::sites::parameters as site_parameters;
use crate::routes::{cache, resolve_site_with_project, validate_time_range};

use super::types::{ProjectRef, SiteRef};

/// The rollup a resolution keyword names. The single mapping: `Resolution::view` gives the view
/// table and [`bucket_interval`] the matching `time_bucket` width, so no endpoint re-derives either.
#[must_use]
pub fn resolution_of(keyword: &str) -> Option<Resolution> {
    match keyword {
        "hourly" => Some(Resolution::Hourly),
        "daily" => Some(Resolution::Daily),
        "weekly" => Some(Resolution::Weekly),
        "monthly" => Some(Resolution::Monthly),
        _ => None,
    }
}

/// The `time_bucket` width matching a rollup, for queries that bucket raw readings themselves.
#[must_use]
pub fn bucket_interval(resolution: Resolution) -> &'static str {
    match resolution {
        Resolution::Hourly => "1 hour",
        Resolution::Daily => "1 day",
        Resolution::Weekly => "7 days",
        Resolution::Monthly => "1 month",
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AggregatesResponse {
    /// Project this data belongs to
    pub project: Option<ProjectRef>,
    /// Site this data belongs to
    pub site: SiteRef,
    /// Aggregation resolution
    pub resolution: String,
    /// Start of time range
    pub start: DateTime<Utc>,
    /// End of time range
    pub end: DateTime<Utc>,
    /// Array of bucket timestamps
    pub times: Vec<DateTime<Utc>>,
    /// Array of parameters with their aggregated values
    pub parameters: Vec<ParameterAggregateData>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ParameterAggregateData {
    pub id: Uuid,
    /// Global parameter id (the catalog parameter this site_parameter references)
    pub parameter_id: Uuid,
    /// Owning sensor for this series. Only present when `split_by_sensor=true` (null = the
    /// unattributed/legacy group). Absent in the default collapsed response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sensor_id: Option<Uuid>,
    /// Stable parameter code (catalog `code`), used as the CSV/NDJSON column key
    pub code: String,
    pub name: String,
    #[serde(rename = "type")]
    pub sensor_type: String,
    pub units: Option<String>,
    /// Average values array (same length as times)
    pub avg: Vec<Option<f64>>,
    /// Minimum values array
    pub min: Vec<Option<f64>>,
    /// Maximum values array
    pub max: Vec<Option<f64>>,
    /// Count of readings per bucket
    pub count: Vec<i64>,
    /// Maximum severity level per bucket (0=ok, 1=warning, 2=alarm). Only present when alarms=true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_severity: Option<Vec<Option<i16>>>,
    /// Count of flagged readings per bucket (always present).
    pub flagged_count: Vec<i64>,
}

/// One rollup bucket. `sensor_id` is NULL on the collapsed read, which selects it as a literal so
/// both reads share this row shape.
#[derive(Debug, FromQueryResult)]
struct AggregateRow {
    bucket: DateTime<Utc>,
    parameter_id: Uuid,
    sensor_id: Option<Uuid>,
    avg_value: Option<f64>,
    min_value: Option<f64>,
    max_value: Option<f64>,
    count: i64,
}

#[derive(Debug, FromQueryResult)]
struct FlaggedBucketRow {
    bucket: DateTime<Utc>,
    parameter_id: Uuid,
    sensor_id: Option<Uuid>,
    flagged_count: i64,
}

/// A series key: the slot, plus the sensor when the sensor dimension is kept.
type SeriesKey = (Uuid, Option<Uuid>);
type AggTuple = (Option<f64>, Option<f64>, Option<f64>, i64);

#[derive(Debug, Deserialize, Serialize, IntoParams)]
pub struct SiteAggregatesQuery {
    /// Start time (required, ISO 8601)
    pub start: DateTime<Utc>,
    /// End time (required, ISO 8601)
    pub end: DateTime<Utc>,
    /// Filter by sensor types (comma-separated)
    pub sensor_types: Option<String>,
    /// Response format: json (default), ndjson, csv
    #[serde(default = "crate::common::bulk::default_format")]
    pub format: String,
    /// Include alarm severity data (threshold violations)
    pub alarms: Option<bool>,
    /// Return one series per sensor instead of collapsing the sensor dimension. JSON only; each
    /// returned parameter entry carries its `sensor_id` (null = the unattributed group).
    pub split_by_sensor: Option<bool>,
}

/// Everything that shapes an aggregates body, with the query flattened in whole so a new field
/// enters the key by construction.
#[derive(Serialize)]
struct AggregatesCacheKey<'a> {
    resolution: &'a str,
    resolved_format: &'a str,
    /// The split as applied, which is off for the bulk formats whatever the query said.
    effective_split: bool,
    #[serde(flatten)]
    query: &'a SiteAggregatesQuery,
}

/// The export projection, built from the same structs the JSON body serialises.
fn aggregates_table(times: &[DateTime<Utc>], params: &[ParameterAggregateData]) -> Table {
    let mut table = Table::at(times);
    for p in params {
        table.column(format!("{}_avg", p.code), Cells::Float(p.avg.clone()));
        table.column(format!("{}_min", p.code), Cells::Float(p.min.clone()));
        table.column(format!("{}_max", p.code), Cells::Float(p.max.clone()));
        table.column(
            format!("{}_count", p.code),
            Cells::Int(p.count.iter().map(|c| Some(*c)).collect()),
        );
    }
    for p in params {
        table.column(
            format!("{}_parameter_id", p.code),
            Cells::Constant(p.parameter_id.to_string()),
        );
    }
    for p in params {
        table.column(
            format!("{}_flagged_count", p.code),
            Cells::Int(p.flagged_count.iter().map(|c| Some(*c)).collect()),
        );
    }
    if params.iter().any(|p| p.max_severity.is_some()) {
        for p in params {
            let cells = p
                .max_severity
                .as_ref()
                .map(|s| s.iter().map(|v| v.map(i64::from)).collect())
                .unwrap_or_default();
            table.column(format!("{}_max_severity", p.code), Cells::Int(cells));
        }
    }
    table
}

/// Get aggregates for a specific site
///
/// Returns aggregated parameter data for all parameters in the specified site.
/// Supports JSON, CSV, and NDJSON formats. Aggregates cover continuous and derived
/// readings only; grab samples (measurement_type 'spot') are excluded, fetch them at
/// raw resolution via the readings endpoint.
#[utoipa::path(
    get,
    path = "/{site_id}/aggregates/{resolution}",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
        ("resolution" = String, Path, description = "Aggregation resolution: hourly, daily, weekly, monthly"),
        SiteAggregatesQuery
    ),
    responses(
        (status = 200, description = "Aggregates retrieved successfully", body = AggregatesResponse),
        (status = 400, description = "Invalid resolution or query parameters"),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn get_site_aggregates(
    State(state): State<AppState>,
    Path((site_id, resolution)): Path<(String, String)>,
    Query(query): Query<SiteAggregatesQuery>,
    ProjectScope(scope): ProjectScope,
    headers: HeaderMap,
) -> AppResult<Response> {
    let (site, project) = resolve_site_with_project(&state.db, &site_id).await?;

    // Enforce project scope
    if !scope.allows_project_opt(site.project_id) {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }

    let project_ref = project.map(|p| ProjectRef {
        id: p.id,
        name: p.name,
    });

    let site_ref = SiteRef {
        id: site.id,
        name: site.name.clone(),
    };

    let Some(rollup) = resolution_of(resolution.as_str()) else {
        return Err(AppError::BadRequest(format!(
            "Invalid resolution: {resolution}. Must be one of: hourly, daily, weekly, monthly"
        )));
    };

    validate_time_range(query.start, query.end)?;

    let format = bulk::determine_format(&query.format, &headers);

    let mut param_query = site_parameters::Entity::find()
        .filter(site_parameters::Column::IsActive.eq(true))
        .filter(site_parameters::Column::SiteId.eq(site.id));

    if let Some(ref types) = query.sensor_types {
        let type_list: Vec<String> = types.split(',').map(|s| s.trim().to_string()).collect();
        if !type_list.is_empty() {
            param_query = param_query.filter(site_parameters::Column::SensorType.is_in(type_list));
        }
    }

    let params_list = param_query.all(&state.db).await?;
    // Global parameter IDs from site_parameters (readings/aggregates use global parameter_id)
    let param_ids: Vec<Uuid> = params_list.iter().map(|p| p.parameter_id).collect();

    // The catalog rows behind these slots: stable codes for the export column keys, and the units
    // fallback the site detail and the readings endpoint resolve the same way.
    let catalog = site_parameters::catalog_map(&state.db, param_ids.iter().copied()).await?;

    let include_alarms = query.alarms.unwrap_or(false);
    // The per-sensor split is JSON-only: sensors sharing a parameter would collide on the export
    // column key. The effective value is what enters the cache key, not what the query asked for.
    let split = query.split_by_sensor.unwrap_or(false) && format == "json";

    // The site id leads the key so a per-site invalidation can find every entry it owns.
    let cache_key = cache_key::key_for(
        &format!("aggregates:{}", site.id),
        &AggregatesCacheKey {
            resolution: &resolution,
            resolved_format: &format,
            effective_split: split,
            query: &query,
        },
    );

    if format == "json"
        && let Some(cached) =
            cache::get_cached(&state, &cache_key, &param_ids, Some(query.end)).await
    {
        return cache::json_response((*cached).clone(), true);
    }

    let _permit = bulk::acquire_bulk_permit(&format, &state.bulk_semaphore)?;

    if param_ids.is_empty() {
        let empty: (Vec<DateTime<Utc>>, Vec<ParameterAggregateData>) = (Vec::new(), Vec::new());
        let resolution = resolution.clone();
        return series::respond(
            &format,
            empty,
            |(times, params)| aggregates_table(times, params),
            |(times, parameters)| async move {
                Ok(Json(AggregatesResponse {
                    project: project_ref,
                    site: site_ref,
                    resolution,
                    start: query.start,
                    end: query.end,
                    times,
                    parameters,
                })
                .into_response())
            },
        )
        .await;
    }

    // Resolve thresholds via the single engine definition (site → global → parameter default),
    // scoped to this site. Replaces the old ORM fetch that ignored the parameter-default tier.
    use crate::routes::private::alarms::thresholds as alarm_engine;
    let threshold_map: HashMap<Uuid, alarm_engine::ResolvedThreshold> = if include_alarms {
        let (sql, values) =
            alarm_engine::resolve_thresholds_query(Some(site.id), Some(param_ids.clone()))
                .build(sea_orm::sea_query::PostgresQueryBuilder);
        let mut map = HashMap::new();
        for row in state
            .db
            .query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                sql,
                values.0,
            ))
            .await?
        {
            if let Ok(tr) = alarm_engine::ThresholdRow::from_query_result(&row, "") {
                map.insert(
                    tr.parameter_id,
                    alarm_engine::ResolvedThreshold {
                        warning_min: tr.warning_min,
                        warning_max: tr.warning_max,
                        alarm_min: tr.alarm_min,
                        alarm_max: tr.alarm_max,
                    },
                );
            }
        }
        map
    } else {
        HashMap::new()
    };

    // $1 = site_id, $2..=$N+1 = parameter_ids, then start, end.
    let placeholders: Vec<String> = param_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("${}", i + 2))
        .collect();
    let mut base_values: Vec<sea_orm::Value> = vec![site.id.into()];
    base_values.extend(param_ids.iter().map(|id| (*id).into()));
    let start_param = param_ids.len() + 2;
    let end_param = start_param + 1;

    let bind = |extra: &[sea_orm::Value]| -> Vec<sea_orm::Value> {
        let mut values = base_values.clone();
        values.extend_from_slice(extra);
        values
    };
    let window: Vec<sea_orm::Value> = vec![query.start.into(), query.end.into()];

    // The CAGG is grouped by (bucket, site_id, parameter_id, sensor_id) since m20260603_000007.
    // The default read collapses the sensor dimension (count-weighted avg = SUM(sum_value)/SUM(count),
    // MIN/MAX, SUM(count)) and selects a NULL sensor_id; `split_by_sensor` keeps it. One query text
    // either way, so the two reads cannot drift.
    let (sensor_select, sensor_group) = if split {
        ("sensor_id", ", sensor_id")
    } else {
        ("NULL::uuid AS sensor_id", "")
    };
    let sql = format!(
        r"
        SELECT
            bucket,
            parameter_id,
            {sensor_select},
            CASE WHEN SUM(count) > 0 THEN SUM(sum_value) / SUM(count) ELSE NULL END AS avg_value,
            MIN(min_value) AS min_value,
            MAX(max_value) AS max_value,
            SUM(count)::bigint AS count
        FROM {view}
        WHERE site_id = $1
          AND parameter_id IN ({ids})
          AND bucket >= ${start_param}
          AND bucket <= ${end_param}
        GROUP BY bucket, parameter_id{sensor_group}
        ORDER BY bucket ASC, parameter_id ASC{sensor_group}
        ",
        view = rollup.view(),
        ids = placeholders.join(","),
    );

    let rows: Vec<AggregateRow> = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            bind(&window),
        ))
        .await?
        .into_iter()
        .filter_map(|row| AggregateRow::from_query_result(&row, "").ok())
        .collect();

    let flagged_sql = format!(
        r"
        SELECT
            time_bucket('{interval}'::interval, time) AS bucket,
            parameter_id,
            {sensor_select},
            COUNT(*)::bigint AS flagged_count
        FROM readings
        WHERE site_id = $1
          AND parameter_id IN ({ids})
          AND time >= ${start_param}
          AND time <= ${end_param}
          AND is_flagged = TRUE
          AND replicate_index = 0
          AND measurement_type IS DISTINCT FROM 'spot'
        GROUP BY bucket, parameter_id{sensor_group}
        ",
        interval = bucket_interval(rollup),
        ids = placeholders.join(","),
    );

    let flagged_rows: Vec<FlaggedBucketRow> = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &flagged_sql,
            bind(&window),
        ))
        .await?
        .into_iter()
        .filter_map(|row| FlaggedBucketRow::from_query_result(&row, "").ok())
        .collect();

    let mut series_map: BTreeMap<SeriesKey, HashMap<DateTime<Utc>, AggTuple>> = BTreeMap::new();
    let mut flagged_map: HashMap<SeriesKey, HashMap<DateTime<Utc>, i64>> = HashMap::new();
    let mut time_set: BTreeSet<DateTime<Utc>> = BTreeSet::new();

    for row in rows {
        time_set.insert(row.bucket);
        series_map
            .entry((row.parameter_id, row.sensor_id))
            .or_default()
            .insert(
                row.bucket,
                (row.avg_value, row.min_value, row.max_value, row.count),
            );
    }
    for row in flagged_rows {
        flagged_map
            .entry((row.parameter_id, row.sensor_id))
            .or_default()
            .insert(row.bucket, row.flagged_count);
    }

    let times: Vec<DateTime<Utc>> = time_set.into_iter().collect();

    // Which series the response lists. Collapsed reports every configured slot, so a parameter
    // with no data in the window still appears with a null series; the split reports the
    // (parameter, sensor) pairs the rollup actually holds.
    let keys: Vec<SeriesKey> = if split {
        series_map.keys().copied().collect()
    } else {
        params_list.iter().map(|p| (p.parameter_id, None)).collect()
    };

    let param_by_id: HashMap<Uuid, &site_parameters::Model> =
        params_list.iter().map(|p| (p.parameter_id, p)).collect();

    let param_data: Vec<ParameterAggregateData> = keys
        .into_iter()
        .map(|key| {
            let (parameter_id, sensor_id) = key;
            let slot = param_by_id.get(&parameter_id).copied();
            let aggs = series_map.get(&key);
            let flagged = flagged_map.get(&key);
            let threshold = threshold_map.get(&parameter_id);

            let mut avg = Vec::with_capacity(times.len());
            let mut min = Vec::with_capacity(times.len());
            let mut max = Vec::with_capacity(times.len());
            let mut count = Vec::with_capacity(times.len());
            let mut flagged_count = Vec::with_capacity(times.len());
            let mut max_severity = include_alarms.then(|| Vec::with_capacity(times.len()));

            for t in &times {
                flagged_count.push(flagged.and_then(|m| m.get(t).copied()).unwrap_or(0));
                let bucket = aggs.and_then(|m| m.get(t));
                avg.push(bucket.and_then(|b| b.0));
                min.push(bucket.and_then(|b| b.1));
                max.push(bucket.and_then(|b| b.2));
                count.push(bucket.map_or(0, |b| b.3));
                if let Some(severities) = max_severity.as_mut() {
                    severities.push(bucket.and_then(|b| {
                        threshold.map(|th| alarm_engine::severity_of_range(b.1, b.2, th))
                    }));
                }
            }

            let descriptor = slot.map(|p| {
                site_parameters::SlotDescriptor::resolve(p, catalog.get(&p.parameter_id))
            });
            ParameterAggregateData {
                id: slot.map_or(parameter_id, |p| p.id),
                parameter_id,
                sensor_id,
                code: descriptor
                    .as_ref()
                    .map(|d| d.code.clone())
                    .unwrap_or_default(),
                name: descriptor
                    .as_ref()
                    .map(|d| d.slot_name.clone())
                    .unwrap_or_default(),
                sensor_type: descriptor
                    .as_ref()
                    .map(|d| d.sensor_type.clone())
                    .unwrap_or_default(),
                units: descriptor.as_ref().and_then(|d| d.units.clone()),
                avg,
                min,
                max,
                count,
                max_severity,
                flagged_count,
            }
        })
        .collect();

    let max_time = times.last().copied();

    series::respond(
        &format,
        (times, param_data),
        |(times, params)| aggregates_table(times, params),
        |(times, parameters)| async move {
            let response = AggregatesResponse {
                project: project_ref,
                site: site_ref,
                resolution,
                start: query.start,
                end: query.end,
                times,
                parameters,
            };
            cache::cache_and_respond(&state, cache_key, &response, max_time).await
        },
    )
    .await
}
