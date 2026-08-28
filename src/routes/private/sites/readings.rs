use axum::{
    Json,
    extract::{Path, Query, State},
    http::header::HeaderMap,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, QueryOrder, Statement,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::common::series::{self, Cells, Table};
use crate::common::{bulk, cache_key};
use crate::error::{AppError, AppResult};
use crate::routes::private::{readings::samples, sites::parameters as site_parameters};
use crate::routes::{cache, resolve_site_with_project, validate_optional_time_range};

use super::types::{ProjectRef, SiteRef};

/// One reading as the series query returns it. `severity` is NULL unless `alarms=true`.
#[derive(Debug, FromQueryResult)]
struct ReadingRow {
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::FixedOffset>,
    value: f64,
    severity: Option<i16>,
    is_flagged: Option<bool>,
    flag_reason: Option<String>,
    measurement_type: Option<String>,
    sample_id: Option<Uuid>,
    calibration_id: Option<Uuid>,
    standard_curve_id: Option<Uuid>,
}

/// Where a row lands on the response's time axis.
///
/// Replicate groups share a timestamp, so the replicate view is positional and the collapsed view
/// is keyed by time. One derivation either way, so the axis and the columns cannot disagree.
enum RowIndex<'a> {
    Positional,
    ByTime(&'a HashMap<DateTime<Utc>, usize>),
}

impl RowIndex<'_> {
    fn of(&self, position: usize, time: DateTime<Utc>) -> Option<usize> {
        match self {
            Self::Positional => Some(position),
            Self::ByTime(index) => index.get(&time).copied(),
        }
    }
}

/// Which optional annotations the caller asked for.
#[derive(Debug, Clone, Copy)]
struct Annotations {
    alarms: bool,
    flagged: bool,
    measurement_type: bool,
    sample_stats: bool,
    curves: bool,
    origin: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReadingsResponse {
    /// Project this data belongs to
    pub project: Option<ProjectRef>,
    /// Site this data belongs to
    pub site: SiteRef,
    /// Start of time range (null if no data)
    pub start: Option<DateTime<Utc>>,
    /// End of time range (null if no data)
    pub end: Option<DateTime<Utc>>,
    /// Array of timestamps (aligned to 10-minute intervals)
    pub times: Vec<DateTime<Utc>>,
    /// Array of parameters with their values
    pub parameters: Vec<ParameterData>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ParameterData {
    pub id: Uuid,
    /// Global parameter id (the catalog parameter this site_parameter references)
    pub parameter_id: Uuid,
    /// Stable parameter code (catalog `code`), used as the CSV/NDJSON column key
    pub code: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(rename = "type")]
    pub sensor_type: String,
    pub units: Option<String>,
    /// Values array (same length as times, null for missing data)
    pub values: Vec<Option<f64>>,
    /// Severity levels (0=ok, 1=warning, 2=alarm). Only present when alarms=true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severities: Option<Vec<Option<i16>>>,
    /// Boolean flags marking outliers (same length as times). Only present when `include_flagged=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flagged: Option<Vec<Option<bool>>>,
    /// Reasons for flagging (same length as times). Only present when `include_flagged=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flag_reasons: Option<Vec<Option<String>>>,
    /// Per-point measurement type (continuous/spot/derived). Only present when `include_measurement_type=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measurement_types: Option<Vec<Option<String>>>,
    /// Per-point base calibration reference (same length as times). Only present when
    /// `include_curves=true`. Null where no calibration was applied, which is what distinguishes an
    /// unrecorded base from an identity one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibration_ids: Option<Vec<Option<Uuid>>>,
    /// Per-point standard curve reference, applied after the base calibration (same length as
    /// times). Only present when `include_curves=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standard_curve_ids: Option<Vec<Option<Uuid>>>,
    /// Per-point sample stats with individual replicates (same length as times; null where the
    /// point is not a replicate group). Only present when `include_sample_stats=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub samples: Option<Vec<Option<SampleStatOut>>>,
    /// The streams paired into this slot (series-level; per-point exactness is
    /// `/readings/provenance`'s job). Only present when `include_origin=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origins: Option<Vec<OriginRef>>,
}

/// One ingestion channel serving a slot.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OriginRef {
    pub stream_id: Uuid,
    pub source_system: String,
    pub source_key: String,
}

/// One replicate behind a grab-sample point.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ReplicateOut {
    pub replicate_index: i16,
    pub raw_value: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibrated_value: Option<f64>,
    /// The base calibration this replicate was corrected with, null when none was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibration_id: Option<Uuid>,
    /// The standard curve applied on top of the base calibration, null when none was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standard_curve_id: Option<Uuid>,
    pub flagged: bool,
}

/// Sample statistics and replicate values behind one served grab point.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SampleStatOut {
    pub sample_id: Uuid,
    pub n: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdev: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    pub replicates: Vec<ReplicateOut>,
}

#[derive(Debug, Deserialize, Serialize, IntoParams)]
pub struct SiteReadingsQuery {
    /// Start time (optional, ISO 8601). If omitted, returns from earliest data.
    pub start: Option<DateTime<Utc>>,
    /// End time (optional, ISO 8601). If omitted, returns to latest data.
    pub end: Option<DateTime<Utc>>,
    /// Filter by sensor types (comma-separated)
    pub sensor_types: Option<String>,
    /// Filter to a specific subset of parameters (comma-separated UUIDs). If omitted, returns all parameters configured for the site.
    pub parameter_ids: Option<String>,
    /// Response format: json (default), ndjson, csv
    #[serde(default = "crate::common::bulk::default_format")]
    pub format: String,
    /// Include alarm severity data (threshold violations)
    pub alarms: Option<bool>,
    /// Filter by measurement type: continuous, spot, derived. Omit to return all types mixed.
    pub measurement_type: Option<String>,
    /// Include flagged readings with flag metadata (default: true). When false, excludes flagged readings entirely.
    pub include_flagged: Option<bool>,
    /// Add the per-point `<code>_flagged` and `<code>_flag_reason` columns to the CSV/NDJSON
    /// export. JSON carries the flag arrays whenever `include_flagged` is on, which is the
    /// default, so the export columns need their own opt-in to keep the default header stable.
    pub include_flags: Option<bool>,
    /// Include replicate readings (default: false). When false, a replicate group is returned as
    /// one row at its lowest unflagged replicate index, carrying the group's sample mean.
    pub include_replicates: Option<bool>,
    /// Filter by sample ID to retrieve replicates for a specific sample.
    pub sample_id: Option<Uuid>,
    /// Include a per-point measurement_type indicator (continuous/spot/derived) on each parameter.
    pub include_measurement_type: Option<bool>,
    /// Add the per-point `calibration_id` and `standard_curve_id` references, so a served value
    /// states which curves produced it.
    pub include_curves: Option<bool>,
    /// Attach per-point sample statistics (n, mean, stdev, min, max) and the individual
    /// replicate values behind each grab point. Spot data only; one batched lookup.
    pub include_sample_stats: Option<bool>,
    /// Attach each parameter's ingestion origins (the streams paired into the slot).
    pub include_origin: Option<bool>,
}

/// Everything that shapes a readings body. The query is flattened in whole, so a field added to
/// `SiteReadingsQuery` enters the key without anyone remembering to list it.
#[derive(Serialize)]
struct ReadingsCacheKey<'a> {
    effective_start: DateTime<Utc>,
    effective_end: Option<DateTime<Utc>>,
    resolved_format: &'a str,
    #[serde(flatten)]
    query: &'a SiteReadingsQuery,
}

/// The export projection: one column set built from the same `ParameterData` the JSON body
/// serialises, so an opt-in cannot be honoured in one format and dropped in another.
///
/// The value and parameter-id columns are the whole default header, grouped by kind across
/// parameters as they always were; every other kind appears only for the opt-in that asked for it.
fn uuid_cells(ids: Option<&Vec<Option<Uuid>>>) -> Vec<Option<String>> {
    ids.map(|v| v.iter().map(|id| id.map(|id| id.to_string())).collect())
        .unwrap_or_default()
}

fn readings_table(times: &[DateTime<Utc>], params: &[ParameterData], include_flags: bool) -> Table {
    let mut table = Table::at(times);
    for p in params {
        table.column(p.code.clone(), Cells::Float(p.values.clone()));
    }
    for p in params {
        table.column(
            format!("{}_parameter_id", p.code),
            Cells::Constant(p.parameter_id.to_string()),
        );
    }
    if params.iter().any(|p| p.measurement_types.is_some()) {
        for p in params {
            table.column(
                format!("{}_measurement_type", p.code),
                Cells::Text(p.measurement_types.clone().unwrap_or_default()),
            );
        }
    }
    if params.iter().any(|p| p.origins.is_some()) {
        for p in params {
            let sources = p
                .origins
                .as_ref()
                .map(|o| {
                    let mut systems: Vec<&str> =
                        o.iter().map(|r| r.source_system.as_str()).collect();
                    systems.sort_unstable();
                    systems.dedup();
                    systems.join("+")
                })
                .unwrap_or_default();
            table.column(format!("{}_source_system", p.code), Cells::Constant(sources));
        }
    }
    if params.iter().any(|p| p.calibration_ids.is_some()) {
        for p in params {
            table.column(
                format!("{}_calibration_id", p.code),
                Cells::Text(uuid_cells(p.calibration_ids.as_ref())),
            );
        }
        for p in params {
            table.column(
                format!("{}_standard_curve_id", p.code),
                Cells::Text(uuid_cells(p.standard_curve_ids.as_ref())),
            );
        }
    }
    if params.iter().any(|p| p.severities.is_some()) {
        for p in params {
            let cells = p
                .severities
                .as_ref()
                .map(|s| s.iter().map(|v| v.map(i64::from)).collect())
                .unwrap_or_default();
            table.column(format!("{}_severity", p.code), Cells::Int(cells));
        }
    }
    if params.iter().any(|p| p.samples.is_some()) {
        for p in params {
            let stats = p.samples.clone().unwrap_or_default();
            table.column(
                format!("{}_n", p.code),
                Cells::Int(
                    stats
                        .iter()
                        .map(|s| s.as_ref().map(|s| i64::from(s.n)))
                        .collect(),
                ),
            );
            table.column(
                format!("{}_mean", p.code),
                Cells::Float(
                    stats
                        .iter()
                        .map(|s| s.as_ref().and_then(|s| s.mean))
                        .collect(),
                ),
            );
            table.column(
                format!("{}_sd", p.code),
                Cells::Float(
                    stats
                        .iter()
                        .map(|s| s.as_ref().and_then(|s| s.stdev))
                        .collect(),
                ),
            );
            table.column(
                format!("{}_min", p.code),
                Cells::Float(
                    stats
                        .iter()
                        .map(|s| s.as_ref().and_then(|s| s.min))
                        .collect(),
                ),
            );
            table.column(
                format!("{}_max", p.code),
                Cells::Float(
                    stats
                        .iter()
                        .map(|s| s.as_ref().and_then(|s| s.max))
                        .collect(),
                ),
            );
        }
    }
    if include_flags {
        for p in params {
            table.column(
                format!("{}_flagged", p.code),
                Cells::Bool(p.flagged.clone().unwrap_or_default()),
            );
        }
        for p in params {
            table.column(
                format!("{}_flag_reason", p.code),
                Cells::Text(p.flag_reasons.clone().unwrap_or_default()),
            );
        }
    }
    table
}

/// Get readings for a specific site
///
/// Returns time-series data for all parameters in the specified site.
/// Supports JSON, CSV, and NDJSON formats.
#[utoipa::path(
    get,
    path = "/{site_id}/readings",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
        SiteReadingsQuery
    ),
    responses(
        (status = 200, description = "Readings retrieved successfully", body = ReadingsResponse),
        (status = 400, description = "Invalid query parameters"),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn get_site_readings(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
    Query(query): Query<SiteReadingsQuery>,
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

    let effective_start = query.start.unwrap_or_else(|| {
        chrono::Utc::now() - chrono::Duration::days(state.config.default_readings_lookback_days)
    });
    let effective_end = query.end;
    validate_optional_time_range(Some(effective_start), effective_end)?;

    // Determine format from query or Accept header
    let format = bulk::determine_format(&query.format, &headers);

    // Build site_parameter query for this site only
    let mut param_query = site_parameters::Entity::find()
        .filter(site_parameters::Column::IsActive.eq(true))
        .filter(site_parameters::Column::SiteId.eq(site.id));

    if let Some(ref types) = query.sensor_types {
        let type_list: Vec<String> = types.split(',').map(|s| s.trim().to_string()).collect();
        if !type_list.is_empty() {
            param_query = param_query.filter(site_parameters::Column::SensorType.is_in(type_list));
        }
    }

    if let Some(ref ids) = query.parameter_ids {
        let parsed: Vec<Uuid> = ids
            .split(',')
            .filter_map(|s| Uuid::parse_str(s.trim()).ok())
            .collect();
        if parsed.is_empty() {
            return Err(AppError::BadRequest(
                "parameter_ids was provided but no UUIDs could be parsed".to_string(),
            ));
        }
        param_query = param_query.filter(site_parameters::Column::ParameterId.is_in(parsed));
    }

    // Get matching site_parameters (needed for cache key validation)
    let params_list = param_query
        .order_by_asc(site_parameters::Column::Name)
        .all(&state.db)
        .await?;

    // Global parameter IDs from site_parameters (readings table uses global parameter_id)
    let param_ids: Vec<Uuid> = params_list.iter().map(|p| p.parameter_id).collect();

    // The catalog rows behind these slots. One resolver decides code, names, sensor_type and the
    // units fallback for every series endpoint, so a slot without its own `display_units` reports
    // the catalog default here exactly as the site detail does.
    let catalog = site_parameters::catalog_map(&state.db, param_ids.iter().copied()).await?;

    let include_replicates = query.include_replicates.unwrap_or(false) || query.sample_id.is_some();
    let annotations = Annotations {
        alarms: query.alarms.unwrap_or(false),
        flagged: query.include_flagged.unwrap_or(true),
        measurement_type: query.include_measurement_type.unwrap_or(false),
        sample_stats: query.include_sample_stats.unwrap_or(false) && !include_replicates,
        curves: query.include_curves.unwrap_or(false),
        origin: query.include_origin.unwrap_or(false),
    };
    let include_flags = query.include_flags.unwrap_or(false);

    if let Some(mt) = query.measurement_type.as_deref()
        && !mt.is_empty()
    {
        crate::routes::private::readings::measurement::validate_measurement_type(Some(mt))?;
    }
    let measurement_type_filter = query.measurement_type.as_deref().unwrap_or("");

    // The site id leads the key so a per-site invalidation can find every entry it owns.
    let cache_key = cache_key::key_for(
        &format!("readings:{}", site.id),
        &ReadingsCacheKey {
            effective_start,
            effective_end,
            resolved_format: &format,
            query: &query,
        },
    );

    // Check cache with freshness validation (JSON only)
    if format == "json"
        && let Some(cached) = cache::get_cached(&state, &cache_key, &param_ids, effective_end).await
    {
        return cache::json_response((*cached).clone(), true);
    }

    let _permit = bulk::acquire_bulk_permit(&format, &state.bulk_semaphore)?;

    if params_list.is_empty() {
        let empty: (Vec<DateTime<Utc>>, Vec<ParameterData>) = (Vec::new(), Vec::new());
        return series::respond(
            &format,
            empty,
            |(times, params)| readings_table(times, params, include_flags),
            |(times, parameters)| async move {
                Ok(Json(ReadingsResponse {
                    project: project_ref,
                    site: site_ref,
                    start: None,
                    end: None,
                    times,
                    parameters,
                })
                .into_response())
            },
        )
        .await;
    }

    let num_params = params_list.len();

    // Build parameterized raw SQL query
    // $1 = site_id, $2..=$N+1 = parameter_ids
    let mut values: Vec<sea_orm::Value> = vec![site.id.into()];
    let placeholders: Vec<String> = param_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("${}", i + 2))
        .collect();
    values.extend(param_ids.iter().map(|id| (*id).into()));

    // One row shape either way: severity is selected as NULL when the caller did not ask for it,
    // so the projection below has a single collection loop rather than one per select.
    // Severity comes from the one shared ladder (alarms engine). NULL when the slot has no
    // threshold at any tier (no `t` row); otherwise the ladder treats all-NULL bounds as 0
    // (disabled).
    let severity_expr = |value_expr: &str| -> String {
        if annotations.alarms {
            let sev = crate::routes::private::alarms::thresholds::severity_case(
                value_expr,
                "t.warning_min",
                "t.warning_max",
                "t.alarm_min",
                "t.alarm_max",
            );
            format!("CASE WHEN t.parameter_id IS NULL THEN NULL ELSE ({sev})::smallint END")
        } else {
            "NULL::smallint".to_string()
        }
    };
    // The 3-tier threshold per slot via the single engine definition (site → global → parameter
    // default), scoped to this site and LEFT JOINed, so a parameter with only defaults still gets
    // a severity (the old direct join to alarm_thresholds did not).
    let threshold_cte = annotations.alarms.then(|| {
        crate::routes::private::alarms::thresholds::resolve_thresholds_sql(
            Some(site.id),
            Some(param_ids.clone()),
        )
    });

    let next_param = param_ids.len() + 2;
    let time_conditions = match effective_end {
        Some(end) => {
            let cond = format!(
                " AND r.time >= ${} AND r.time <= ${}",
                next_param,
                next_param + 1
            );
            values.push(effective_start.into());
            values.push(end.into());
            cond
        }
        None => {
            let cond = format!(" AND r.time >= ${next_param}");
            values.push(effective_start.into());
            cond
        }
    };

    let flagged_condition = if annotations.flagged {
        ""
    } else {
        " AND (r.is_flagged IS NOT TRUE)"
    };

    let sample_id_condition = if let Some(sid) = query.sample_id {
        let idx = values.len() + 1;
        values.push(sid.into());
        format!(" AND r.sample_id = ${idx}")
    } else {
        String::new()
    };

    let placeholders = placeholders.join(",");
    let sql = if include_replicates {
        // Every stored row, one per replicate; the caller reconstructs the groups.
        // "continuous" means everything that is not a grab: derived rows plot on the continuous
        // line (matching the continuous aggregates, which exclude only 'spot'), and legacy NULL
        // rows predate the measurement_type column.
        let measurement_type_condition = match measurement_type_filter {
            "" => String::new(),
            "continuous" => " AND (r.measurement_type IS DISTINCT FROM 'spot')".to_string(),
            other => {
                let idx = values.len() + 1;
                values.push(other.to_string().into());
                format!(" AND r.measurement_type = ${idx}")
            }
        };
        let severity = severity_expr("COALESCE(r.calibrated_value, r.raw_value)");
        let select_clause = format!(
            "r.parameter_id, r.time, COALESCE(r.calibrated_value, r.raw_value) AS value, \
             {severity} AS severity, r.is_flagged, r.flag_reason, r.measurement_type, \
             r.sample_id, r.calibration_id, r.standard_curve_id"
        );
        let from_clause = match &threshold_cte {
            Some(cte) => format!(
                "readings r LEFT JOIN ({cte}) t \
                 ON t.parameter_id = r.parameter_id AND t.site_id = r.site_id"
            ),
            None => "readings r".to_string(),
        };
        format!(
            "SELECT {select_clause} FROM {from_clause} \
             WHERE r.site_id = $1 AND r.parameter_id IN ({placeholders})\
             {time_conditions}{measurement_type_condition}{flagged_condition}{sample_id_condition} \
             ORDER BY r.parameter_id, r.time, r.replicate_index"
        )
    } else {
        // Continuous and derived rows live at replicate_index 0 (every continuous writer defaults
        // to it), so the plain equality keeps the ordered scan. A spot instant is the replicate
        // group `(stream_id, time)`, served at the sample mean over its unflagged replicates with
        // the lowest unflagged replicate's own value as the no-sample fallback; the DISTINCT ON
        // is confined to the spot subset, whose row counts are small. "continuous" folds in
        // derived and legacy NULL rows (matching the continuous aggregates, which exclude only
        // 'spot'); any other named type is continuous-shaped and narrows the continuous arm.
        let (include_continuous_arm, include_spot_arm, continuous_extra) =
            match measurement_type_filter {
                "" => (true, true, String::new()),
                "continuous" => (true, false, String::new()),
                "spot" => (false, true, String::new()),
                other => {
                    let idx = values.len() + 1;
                    values.push(other.to_string().into());
                    (true, false, format!(" AND r.measurement_type = ${idx}"))
                }
            };
        let base_cols = "r.parameter_id, r.time, r.site_id, r.is_flagged, r.flag_reason, \
             r.measurement_type, r.sample_id, r.calibration_id, r.standard_curve_id";
        let mut arms: Vec<String> = Vec::new();
        if include_continuous_arm {
            arms.push(format!(
                "SELECT COALESCE(r.calibrated_value, r.raw_value) AS value, {base_cols} \
                 FROM readings r \
                 WHERE r.site_id = $1 AND r.parameter_id IN ({placeholders}) \
                   AND r.replicate_index = 0 AND r.measurement_type IS DISTINCT FROM 'spot'\
                 {time_conditions}{continuous_extra}{flagged_condition}{sample_id_condition}"
            ));
        }
        if include_spot_arm {
            arms.push(format!(
                "SELECT sp.* FROM ( \
                    SELECT DISTINCT ON (r.stream_id, r.time) \
                           COALESCE(smp.mean, r.calibrated_value, r.raw_value) AS value, \
                           {base_cols} \
                    FROM readings r LEFT JOIN samples smp ON smp.id = r.sample_id \
                    WHERE r.site_id = $1 AND r.parameter_id IN ({placeholders}) \
                      AND r.measurement_type = 'spot' AND r.withdrawn_at IS NULL\
                    {time_conditions}{flagged_condition}{sample_id_condition} \
                    ORDER BY r.stream_id, r.time, (r.is_flagged IS TRUE), r.replicate_index \
                 ) sp"
            ));
        }
        let inner = arms.join(" UNION ALL ");
        let severity = severity_expr("sv.value");
        let threshold_join = match &threshold_cte {
            Some(cte) => format!(
                " LEFT JOIN ({cte}) t \
                 ON t.parameter_id = sv.parameter_id AND t.site_id = sv.site_id"
            ),
            None => String::new(),
        };
        format!(
            "SELECT sv.parameter_id, sv.time, sv.value, {severity} AS severity, \
                    sv.is_flagged, sv.flag_reason, sv.measurement_type, sv.sample_id, \
                    sv.calibration_id, sv.standard_curve_id \
             FROM ({inner}) sv{threshold_join} \
             ORDER BY sv.parameter_id, sv.time"
        )
    };

    let query_result = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?;

    let estimated_times = query_result.len() / num_params.max(1);

    let mut param_rows: HashMap<Uuid, Vec<ReadingRow>> = HashMap::with_capacity(num_params);
    for row in &query_result {
        if let Ok(r) = ReadingRow::from_query_result(row, "") {
            param_rows
                .entry(r.parameter_id)
                .or_insert_with(|| Vec::with_capacity(estimated_times))
                .push(r);
        }
    }

    // One derivation of the time axis, and the row-to-column mapping that goes with it. Replicates
    // share timestamps, so that view is positional and takes its axis from the parameter with the
    // most rows; every other view is keyed by time.
    let times: Vec<DateTime<Utc>> = if include_replicates {
        param_rows
            .values()
            .max_by_key(|rows| rows.len())
            .map(|rows| rows.iter().map(|r| r.time.with_timezone(&Utc)).collect())
            .unwrap_or_default()
    } else {
        let mut set: HashSet<DateTime<Utc>> = HashSet::with_capacity(estimated_times);
        for rows in param_rows.values() {
            for r in rows {
                set.insert(r.time.with_timezone(&Utc));
            }
        }
        let mut times: Vec<DateTime<Utc>> = set.into_iter().collect();
        times.sort_unstable();
        times
    };

    let time_index: HashMap<DateTime<Utc>, usize> =
        times.iter().enumerate().map(|(i, t)| (*t, i)).collect();
    let index = if include_replicates {
        RowIndex::Positional
    } else {
        RowIndex::ByTime(&time_index)
    };

    // One batched lookup resolves every referenced sample and its replicate readings
    let sample_stats: HashMap<Uuid, SampleStatOut> = if annotations.sample_stats {
        let ids: Vec<Uuid> = param_rows
            .values()
            .flat_map(|rows| rows.iter().filter_map(|r| r.sample_id))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        fetch_sample_stats(&state.db, &ids, effective_start, effective_end).await?
    } else {
        HashMap::new()
    };

    let origin_map: HashMap<Uuid, Vec<OriginRef>> = if annotations.origin {
        use crate::routes::private::data_streams;
        let sp_ids: Vec<Uuid> = params_list.iter().map(|sp| sp.id).collect();
        let mut map: HashMap<Uuid, Vec<OriginRef>> = HashMap::new();
        for stream in data_streams::Entity::find()
            .filter(data_streams::Column::SiteParameterId.is_in(sp_ids))
            .all(&state.db)
            .await?
        {
            if let Some(sp_id) = stream.site_parameter_id {
                map.entry(sp_id).or_default().push(OriginRef {
                    stream_id: stream.id,
                    source_system: stream.source_system,
                    source_key: stream.source_key,
                });
            }
        }
        map
    } else {
        HashMap::new()
    };

    let param_data: Vec<ParameterData> = params_list
        .iter()
        .map(|sp| {
            let len = times.len();
            let mut values: Vec<Option<f64>> = vec![None; len];
            let mut severities = annotations.alarms.then(|| vec![None; len]);
            let mut flagged = annotations.flagged.then(|| vec![None; len]);
            let mut flag_reasons = annotations.flagged.then(|| vec![None; len]);
            let mut measurement_types = annotations.measurement_type.then(|| vec![None; len]);
            let mut calibration_ids = annotations.curves.then(|| vec![None; len]);
            let mut standard_curve_ids = annotations.curves.then(|| vec![None; len]);
            let mut samples = annotations.sample_stats.then(|| vec![None; len]);

            if let Some(rows) = param_rows.get(&sp.parameter_id) {
                for (position, row) in rows.iter().enumerate() {
                    let Some(i) = index.of(position, row.time.with_timezone(&Utc)) else {
                        continue;
                    };
                    if i >= len {
                        continue;
                    }
                    values[i] = Some(row.value);
                    if let Some(v) = severities.as_mut() {
                        v[i] = row.severity;
                    }
                    if let Some(v) = flagged.as_mut() {
                        v[i] = row.is_flagged;
                    }
                    if let Some(v) = flag_reasons.as_mut() {
                        v[i] = row.flag_reason.clone();
                    }
                    if let Some(v) = measurement_types.as_mut() {
                        v[i] = row.measurement_type.clone();
                    }
                    if let Some(v) = calibration_ids.as_mut() {
                        v[i] = row.calibration_id;
                    }
                    if let Some(v) = standard_curve_ids.as_mut() {
                        v[i] = row.standard_curve_id;
                    }
                    if let Some(v) = samples.as_mut() {
                        v[i] = row
                            .sample_id
                            .and_then(|sid| sample_stats.get(&sid).cloned());
                    }
                }
            }

            let descriptor =
                site_parameters::SlotDescriptor::resolve(sp, catalog.get(&sp.parameter_id));
            ParameterData {
                id: sp.id,
                parameter_id: sp.parameter_id,
                code: descriptor.code,
                name: descriptor.slot_name,
                display_name: descriptor.catalog_name,
                sensor_type: descriptor.sensor_type,
                units: descriptor.units,
                values,
                severities,
                flagged,
                flag_reasons,
                measurement_types,
                calibration_ids,
                standard_curve_ids,
                samples,
                origins: annotations
                    .origin
                    .then(|| origin_map.get(&sp.id).cloned().unwrap_or_default()),
            }
        })
        .collect();

    let actual_start = times.first().copied();
    let actual_end = times.last().copied();

    series::respond(
        &format,
        (times, param_data),
        |(times, params)| readings_table(times, params, include_flags),
        |(times, parameters)| async move {
            let response = ReadingsResponse {
                project: project_ref,
                site: site_ref,
                start: actual_start,
                end: actual_end,
                times,
                parameters,
            };
            cache::cache_and_respond(&state, cache_key, &response, actual_end).await
        },
    )
    .await
}

/// Resolve sample rows and their replicate readings for the given sample ids in two batched
/// queries, keyed by sample id.
async fn fetch_sample_stats(
    db: &sea_orm::DatabaseConnection,
    sample_ids: &[Uuid],
    start: DateTime<Utc>,
    end: Option<DateTime<Utc>>,
) -> Result<HashMap<Uuid, SampleStatOut>, AppError> {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    if sample_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut stats: HashMap<Uuid, SampleStatOut> = samples::Entity::find()
        .filter(samples::Column::Id.is_in(sample_ids.to_vec()))
        .all(db)
        .await?
        .into_iter()
        .map(|s| {
            (
                s.id,
                SampleStatOut {
                    sample_id: s.id,
                    n: s.n,
                    mean: s.mean,
                    stdev: s.stdev,
                    min: s.min_value,
                    max: s.max_value,
                    replicates: Vec::new(),
                },
            )
        })
        .collect();

    // The time bounds keep chunk exclusion in play; sample_id alone plans across every chunk.
    let (time_clause, mut values) = match end {
        Some(e) => (
            "AND time >= $2 AND time <= $3",
            vec![sample_ids.to_vec().into(), start.into(), e.into()],
        ),
        None => (
            "AND time >= $2",
            vec![sample_ids.to_vec().into(), start.into()],
        ),
    };
    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT sample_id, replicate_index, raw_value, calibrated_value, is_flagged, \
                 calibration_id, standard_curve_id \
                 FROM readings WHERE sample_id = ANY($1) {time_clause} \
                 ORDER BY sample_id, replicate_index"
            ),
            std::mem::take(&mut values),
        ))
        .await?;
    for row in rows {
        let Ok(sid) = row.try_get::<Uuid>("", "sample_id") else {
            continue;
        };
        if let Some(stat) = stats.get_mut(&sid) {
            stat.replicates.push(ReplicateOut {
                replicate_index: row.try_get("", "replicate_index").unwrap_or(0),
                raw_value: row.try_get("", "raw_value").unwrap_or(f64::NAN),
                calibrated_value: row.try_get("", "calibrated_value").ok(),
                calibration_id: row.try_get("", "calibration_id").ok(),
                standard_curve_id: row.try_get("", "standard_curve_id").ok(),
                flagged: row
                    .try_get::<Option<bool>>("", "is_flagged")
                    .ok()
                    .flatten()
                    .unwrap_or(false),
            });
        }
    }

    Ok(stats)
}
