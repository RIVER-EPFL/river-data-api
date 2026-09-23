//! The site-scoped handlers and this component's router.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use axum::{
    Json,
    extract::{Path, Query, State},
    http::header::{self, HeaderMap, HeaderValue},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, Utc};
use sea_orm::sea_query::{
    Alias, Condition, Expr, Func, JoinType, PostgresQueryBuilder, Query as SeaQuery,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, ExprTrait, FromQueryResult, Order, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, Statement,
};
use utoipa_axum::router::OpenApiRouter;
use uuid::Uuid;

use super::models::*;
use super::service::*;
use crate::common::authz::{AccessScope, Capability, TokenAccess};
use crate::common::csv::field as csv_field;
use crate::common::middleware::{
    ProjectScope, require_crud, require_read_data, require_read_metadata,
};
use crate::common::paging::Window;
use crate::common::series;
use crate::common::served;
use crate::common::{AppState, bulk, cache_key};
use crate::error::{AppError, AppResult};
use crate::routes::private::annotations::{self, Annotation};
use crate::routes::private::data_streams::models as data_streams;
use crate::routes::private::meteoswiss::service as meteoswiss;
use crate::routes::private::parameters;
use crate::routes::private::readings::models as readings;
use crate::routes::private::readings::samples::models as samples;
use crate::routes::private::readings::status_events::models as status_events;
use crate::routes::private::site_parameters;
use crate::routes::{
    cache, resolve_site, resolve_site_with_project, validate_optional_time_range,
    validate_time_range,
};

// --- Site detail and the parameter list ---

/// List parameters for a site
#[utoipa::path(
    get,
    path = "/api/sites/{site_id}/parameters",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
    ),
    responses(
        (status = 200, description = "Parameters retrieved successfully", body = Vec<ParameterResponse>),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn list_site_parameters(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<Vec<ParameterResponse>>> {
    let site = resolve_site(&state.db, &site_id).await?;

    require_site_in_scope(&scope, &site)?;

    let params_list = site_parameters::Entity::find()
        .filter(site_parameters::Column::SiteId.eq(site.id))
        .filter(site_parameters::Column::IsActive.eq(true))
        .order_by_asc(site_parameters::Column::Name)
        .all(&state.db)
        .await?;

    let param_ids: Vec<Uuid> = params_list.iter().map(|p| p.parameter_id).collect();
    let globals = site_parameters::catalog_map(&state.db, param_ids.iter().copied()).await?;
    let extents = parameter_extents(&state.db, site.id).await?;
    let attributions = meteoswiss::site_attributions(&state.db, site.id).await?;

    let response: Vec<ParameterResponse> = params_list
        .into_iter()
        .map(|p| build_parameter_response(p, &globals, &extents, &attributions))
        .collect();

    Ok(Json(response))
}

/// Get detailed site information including project, parameters, and data range
#[utoipa::path(
    get,
    path = "/api/sites/{site_id}/detail",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
    ),
    responses(
        (status = 200, description = "Site detail retrieved successfully", body = SiteDetailResponse),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn get_site_detail(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<SiteDetailResponse>> {
    let (site, project) = resolve_site_with_project(&state.db, &site_id).await?;

    require_site_in_scope(&scope, &site)?;

    // Query active parameters
    let params_list = site_parameters::Entity::find()
        .filter(site_parameters::Column::SiteId.eq(site.id))
        .filter(site_parameters::Column::IsActive.eq(true))
        .order_by_asc(site_parameters::Column::Name)
        .all(&state.db)
        .await?;

    let param_ids: Vec<Uuid> = params_list.iter().map(|p| p.parameter_id).collect();
    let globals = site_parameters::catalog_map(&state.db, param_ids.iter().copied()).await?;
    let extents = parameter_extents(&state.db, site.id).await?;
    let attributions = meteoswiss::site_attributions(&state.db, site.id).await?;

    let parameters: Vec<ParameterResponse> = params_list
        .into_iter()
        .map(|p| build_parameter_response(p, &globals, &extents, &attributions))
        .collect();

    // The extents cover the same rows this range spans (`WHERE site_id = $1`), so folding them is
    // the ungrouped aggregate without scanning the hypertable again.
    let (data_start, data_end, reading_count) = extents.values().fold(
        (None, None, 0i64),
        |(start, end, count): (Option<DateTime<Utc>>, Option<DateTime<Utc>>, i64), e| {
            (
                min_opt(start, e.data_start),
                max_opt(end, e.data_end),
                count + e.reading_count,
            )
        },
    );

    Ok(Json(SiteDetailResponse {
        id: site.id,
        name: site.name,
        latitude: site.latitude,
        longitude: site.longitude,
        altitude_m: site.altitude_m,
        project: project.map(|p| ProjectRef {
            id: p.id,
            name: p.name,
        }),
        parameters,
        data_start,
        data_end,
        reading_count,
    }))
}

// --- Readings ---

/// Get readings for a specific site
///
/// Returns time-series data for all parameters in the specified site.
/// Supports JSON, CSV, and NDJSON formats.
#[utoipa::path(
    get,
    path = "/api/sites/{site_id}/readings",
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
    let db = &state.db;
    let (site, project) = resolve_site_with_project(db, &site_id).await?;
    require_site_in_scope(&scope, &site)?;
    let request = ReadingsRequest::from_query(
        &query,
        state.config.default_readings_lookback_days,
        Utc::now(),
    )?;
    let format = bulk::determine_format(&query.format, &headers);
    let slots = requested_slots(db, site.id, &query).await?;
    let param_ids: Vec<Uuid> = slots.iter().map(|sp| sp.parameter_id).collect();
    let cache_key = readings_cache_key(site.id, &request, &format, &query);
    if format == "json"
        && let Some(cached) = cache::get_cached(&state, &cache_key, &param_ids, request.end).await
    {
        return cache::json_response((*cached).clone(), true);
    }
    let _permit = bulk::acquire_bulk_permit(&format, &state.bulk_semaphore)?;
    let (project, site_ref) = readings_owner(&site, project);
    if slots.is_empty() {
        let empty = empty_readings_response(project, site_ref);
        return respond_readings(&state, &format, None, empty, request.flag_columns).await;
    }
    let catalog = site_parameters::catalog_map(db, param_ids.iter().copied()).await?;
    let rows = served_rows(db, site.id, &param_ids, &request).await?;
    let axis = RowAxis::over(&rows, request.replicates);
    let context = series_context(db, site.id, &slots, &rows, &request).await?;
    let parameters = slot_series(&slots, &catalog, rows, &axis, request.annotations, &context);
    let response = readings_response(project, site_ref, &axis, parameters);
    respond_readings(
        &state,
        &format,
        Some(cache_key),
        response,
        request.flag_columns,
    )
    .await
}

/// The project and site a readings body names.
fn readings_owner(
    site: &Model,
    project: Option<crate::routes::private::projects::Model>,
) -> (Option<ProjectRef>, SiteRef) {
    let project = project.map(|p| ProjectRef {
        id: p.id,
        name: p.name,
    });
    let site = SiteRef {
        id: site.id,
        name: site.name.clone(),
    };
    (project, site)
}

/// Refuses a caller whose scope does not reach the site's project.
fn require_site_in_scope(scope: &AccessScope, site: &Model) -> AppResult<()> {
    if scope.allows_project_opt(site.project_id) {
        return Ok(());
    }
    Err(AppError::Forbidden(
        "Token is scoped to a different project".to_string(),
    ))
}

/// The site's active slots the query's sensor-type and parameter filters select, by name.
async fn requested_slots(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    query: &SiteReadingsQuery,
) -> AppResult<Vec<site_parameters::Model>> {
    let mut slots = site_parameters::Entity::find()
        .filter(site_parameters::Column::IsActive.eq(true))
        .filter(site_parameters::Column::SiteId.eq(site_id));
    if let Some(types) = sensor_type_filter(query.sensor_types.as_deref()) {
        slots = slots.filter(site_parameters::Column::SensorType.is_in(types));
    }
    if let Some(ids) = parameter_id_filter(query.parameter_ids.as_deref())? {
        slots = slots.filter(site_parameters::Column::ParameterId.is_in(ids));
    }
    Ok(slots
        .order_by_asc(site_parameters::Column::Name)
        .all(db)
        .await?)
}

/// The cache key of a readings body. The site id leads it so a per-site invalidation can find
/// every entry it owns.
fn readings_cache_key(
    site_id: Uuid,
    request: &ReadingsRequest,
    format: &str,
    query: &SiteReadingsQuery,
) -> String {
    cache_key::key_for(
        &format!("readings:{site_id}"),
        &ReadingsCacheKey {
            effective_start: request.start,
            effective_end: request.end,
            resolved_format: format,
            query,
        },
    )
}

/// The rows the request serves: one per replicate, or one per continuous instant and per spot
/// instant.
async fn served_rows(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    param_ids: &[Uuid],
    request: &ReadingsRequest,
) -> AppResult<Vec<ReadingRow>> {
    let statement = if request.replicates {
        replicate_rows_query(site_id, param_ids, request)
    } else {
        served_series_query(site_id, param_ids, request)
    };
    let rows: Vec<ReadingRow> = db
        .query_all_raw(sea_orm::DatabaseBackend::Postgres.build(&statement))
        .await?
        .iter()
        .filter_map(|row| ReadingRow::from_query_result(row, "").ok())
        .collect();
    if request.replicates {
        return Ok(rows);
    }
    one_row_per_continuous_instant(db, site_id, param_ids, rows, request.annotations.alarms).await
}

/// The sample statistics, stream origins and withdrawn counts the request asked for.
async fn series_context(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    slots: &[site_parameters::Model],
    rows: &[ReadingRow],
    request: &ReadingsRequest,
) -> AppResult<SeriesContext> {
    Ok(SeriesContext {
        sample_stats: sample_stats_of(db, rows, request).await?,
        origins: slot_origins(db, slots, request.annotations.origin).await?,
        withdrawn_counts: withdrawn_counts_of(db, site_id, slots, request).await?,
    })
}

/// Every sample the rows reference with its replicate readings, in one batched lookup, when the
/// request asked for them.
async fn sample_stats_of(
    db: &sea_orm::DatabaseConnection,
    rows: &[ReadingRow],
    request: &ReadingsRequest,
) -> AppResult<HashMap<Uuid, SampleStatOut>> {
    if !request.annotations.sample_stats {
        return Ok(HashMap::new());
    }
    let ids: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| r.sample_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    fetch_sample_stats(db, &ids, request.start, request.end).await
}

/// The streams paired into each slot, keyed by site-parameter id, when the request asked for them.
async fn slot_origins(
    db: &sea_orm::DatabaseConnection,
    slots: &[site_parameters::Model],
    asked: bool,
) -> AppResult<Option<HashMap<Uuid, Vec<OriginRef>>>> {
    if !asked {
        return Ok(None);
    }
    let slot_ids: Vec<Uuid> = slots.iter().map(|sp| sp.id).collect();
    let mut origins: HashMap<Uuid, Vec<OriginRef>> = HashMap::new();
    for stream in data_streams::Entity::find()
        .filter(data_streams::Column::SiteParameterId.is_in(slot_ids))
        .all(db)
        .await?
    {
        if let Some(slot_id) = stream.site_parameter_id {
            origins.entry(slot_id).or_default().push(OriginRef {
                stream_id: stream.id,
                source_system: stream.source_system,
                source_key: stream.source_key,
            });
        }
    }
    Ok(Some(origins))
}

/// Each parameter's fully withdrawn spot instants in the window, when the request covers them.
/// Only instants no live replicate survives on are counted: one retracted replicate is not a
/// retracted visit.
async fn withdrawn_counts_of(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    slots: &[site_parameters::Model],
    request: &ReadingsRequest,
) -> AppResult<Option<HashMap<Uuid, i64>>> {
    if !request.counts_withdrawn() {
        return Ok(None);
    }
    let parameter_ids: Vec<Uuid> = slots.iter().map(|sp| sp.parameter_id).collect();
    let counts =
        count_withdrawn_instants(db, site_id, &parameter_ids, request.start, request.end).await?;
    Ok(Some(counts))
}

/// The body as JSON, CSV or NDJSON. A JSON body under a cache key is cached until its last
/// instant is superseded; CSV and NDJSON are one column set built from the same series.
async fn respond_readings(
    state: &AppState,
    format: &str,
    cache_key: Option<String>,
    response: ReadingsResponse,
    flag_columns: bool,
) -> AppResult<Response> {
    series::respond(
        format,
        response,
        |r| {
            readings_table(
                &r.times,
                &r.parameters,
                flag_columns,
                r.replicate_indices.as_deref(),
            )
        },
        |r| async move {
            match cache_key {
                Some(key) => cache::cache_and_respond(state, key, &r, r.end).await,
                None => Ok(Json(r).into_response()),
            }
        },
    )
    .await
}

/// The site chart's rows with each continuous instant several streams feed served once, at the
/// mean of what it pools (`common::served`). A pooled value is a value the query never saw, so
/// its severity is classified here against the same thresholds.
async fn one_row_per_continuous_instant(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    param_ids: &[Uuid],
    rows: Vec<ReadingRow>,
    alarms: bool,
) -> AppResult<Vec<ReadingRow>> {
    let mut pooled_at: HashSet<(Uuid, DateTime<chrono::FixedOffset>)> = HashSet::new();
    let mut rows = served::one_row_per_continuous_instant(
        rows,
        |row| {
            (row.measurement_type.as_deref() != Some("spot")).then_some((
                (row.parameter_id, row.time),
                served::InstantMember {
                    value: row.value,
                    flagged: row.is_flagged == Some(true),
                    unverified: row.unverified == Some(true),
                    stream_id: row.stream_id,
                },
            ))
        },
        |row, pool| {
            row.value = pool.value;
            pooled_at.insert((row.parameter_id, row.time));
        },
    );
    if alarms && !pooled_at.is_empty() {
        let thresholds = resolved_thresholds(db, site_id, param_ids).await?;
        for row in &mut rows {
            if pooled_at.contains(&(row.parameter_id, row.time))
                && row.measurement_type.as_deref() != Some("spot")
            {
                row.severity = thresholds
                    .get(&row.parameter_id)
                    .map(|t| crate::routes::private::alarms::service::severity_of(row.value, t));
            }
        }
    }
    Ok(rows)
}

/// Each parameter's threshold at this site, through the single engine definition (site, then
/// global, then the parameter default).
async fn resolved_thresholds(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    param_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, crate::routes::private::alarms::models::ResolvedThreshold>> {
    use crate::routes::private::alarms::models as alarm_models;
    let (sql, values) = crate::routes::private::alarms::service::resolve_thresholds_query(
        Some(site_id),
        Some(param_ids.to_vec()),
    )
    .build(PostgresQueryBuilder);
    let mut map = HashMap::new();
    for row in db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values.0,
        ))
        .await?
    {
        if let Ok(tr) = alarm_models::ThresholdRow::from_query_result(&row, "") {
            map.insert(
                tr.parameter_id,
                alarm_models::ResolvedThreshold {
                    warning_min: tr.warning_min,
                    warning_max: tr.warning_max,
                    alarm_min: tr.alarm_min,
                    alarm_max: tr.alarm_max,
                },
            );
        }
    }
    Ok(map)
}

// --- Aggregates ---

/// Get aggregates for a specific site
///
/// Returns aggregated parameter data for all parameters in the specified site.
/// Supports JSON, CSV, and NDJSON formats. Aggregates cover continuous and derived
/// readings only; grab samples (measurement_type 'spot') are excluded, fetch them at
/// raw resolution via the readings endpoint.
#[utoipa::path(
    get,
    path = "/api/sites/{site_id}/aggregates/{resolution}",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
        ("resolution" = String, Path, description = "Aggregation resolution: hourly, 6hourly, 12hourly, daily, weekly, monthly"),
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

    require_site_in_scope(&scope, &site)?;

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
            "Invalid resolution: {resolution}. Must be one of: hourly, 6hourly, 12hourly, daily, weekly, monthly"
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

    use crate::routes::private::alarms::service as alarm_engine;
    let threshold_map = if include_alarms {
        resolved_thresholds(&state.db, site.id, &param_ids).await?
    } else {
        HashMap::new()
    };

    let bounds: Vec<sea_orm::Value> = vec![
        site.id.into(),
        param_ids.to_vec().into(),
        query.start.into(),
        query.end.into(),
    ];

    let rows: Vec<AggregateRow> = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            aggregate_buckets_sql(rollup, split),
            bounds,
        ))
        .await?
        .into_iter()
        .filter_map(|row| AggregateRow::from_query_result(&row, "").ok())
        .collect();

    let (flagged_sql, flagged_values) =
        flagged_buckets_query(rollup, site.id, &param_ids, split, query.start, query.end)
            .build(PostgresQueryBuilder);

    let flagged_rows: Vec<FlaggedBucketRow> = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            flagged_sql,
            flagged_values,
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

            let descriptor = slot
                .map(|p| site_parameters::SlotDescriptor::resolve(p, catalog.get(&p.parameter_id)));
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

// --- Status events ---

/// Get status events for a specific site
///
/// Returns non-numeric time-series events (device status strings, firmware versions)
/// for the specified site. Supports JSON, CSV, and NDJSON formats.
#[utoipa::path(
    get,
    path = "/api/sites/{site_id}/status_events",
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

    require_site_in_scope(&scope, &site)?;

    let site_ref = SiteRef {
        id: site.id,
        name: site.name.clone(),
    };

    validate_optional_time_range(query.start, query.end)?;

    // Determine format from query or Accept header
    let format = bulk::determine_format(&query.format, &headers);

    let _permit = bulk::acquire_bulk_permit(&format, &state.bulk_semaphore)?;

    let mut at_this_site = Condition::all().add(status_events::Column::SiteId.eq(site.id));
    if let Some(start) = query.start {
        at_this_site = at_this_site.add(status_events::Column::Time.gte(start));
    }
    if let Some(end) = query.end {
        at_this_site = at_this_site.add(status_events::Column::Time.lte(end));
    }

    let dir = if query.order.as_deref() == Some("desc") {
        Order::Desc
    } else {
        Order::Asc
    };

    // Pagination applies to JSON only; CSV/NDJSON remain full-range exports.
    let page = match (format.as_str(), query.limit) {
        ("json", Some(limit)) => Some(Window::from_limit_offset(
            Some(limit),
            query.offset,
            1000,
            1000,
        )),
        _ => None,
    };

    // Count the full match set only when a LIMIT truncates the result; otherwise
    // the row count already is the total.
    let counted_total: Option<u64> = match page {
        None => None,
        Some(_) => Some(
            status_events::Entity::find()
                .filter(at_this_site.clone())
                .count(&state.db)
                .await?,
        ),
    };

    // `time` is not unique here (the PK is (stream_id, time), so one timestamp carries one row per
    // stream), and LIMIT/OFFSET over a partial order can repeat or skip a tied row between pages.
    // Ordering by the full key makes the walk a total order.
    let mut rows = status_events::Entity::find()
        .filter(at_this_site)
        .order_by(status_events::Column::Time, dir.clone())
        .order_by(status_events::Column::StreamId, dir);
    if let Some(window) = page {
        rows = rows.limit(window.limit).offset(window.offset);
    }

    let events: Vec<StatusEventData> = rows
        .all(&state.db)
        .await?
        .into_iter()
        // A status event whose slot is unpaired names no parameter and has no column to render
        // under, which is what the row shape said before the entity did.
        .filter_map(|r| {
            r.parameter_id.map(|parameter_id| StatusEventData {
                parameter_id,
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
            let total = counted_total.unwrap_or(events.len() as u64);
            let response = StatusEventsResponse {
                site: site_ref,
                events,
                total,
            };
            Ok(Json(response).into_response())
        }
    }
}

// --- Annotations and the export summary ---

/// List annotations for a site, optionally filtered by parameter and time range
#[utoipa::path(
    get,
    path = "/api/sites/{site_id}/annotations",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
        SiteAnnotationsQuery
    ),
    responses(
        (status = 200, description = "Annotations retrieved successfully", body = Vec<Annotation>),
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

    require_site_in_scope(&scope, &site)?;

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

    let response: Vec<Annotation> = rows.into_iter().map(Annotation::from).collect();

    Ok(Json(response).into_response())
}

#[utoipa::path(
    get,
    path = "/api/sites/{site_id}/export/summary",
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
    require_site_in_scope(&scope, &site)?;
    if query.end <= query.start {
        return Err(AppError::BadRequest("end must be after start".to_string()));
    }
    let start = sea_orm::prelude::DateTimeWithTimeZone::from(query.start);
    let end = sea_orm::prelude::DateTimeWithTimeZone::from(query.end);

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
    let a = Alias::new("a");
    let r_ = Alias::new("r");
    let (sql, values) = SeaQuery::select()
        .expr_as(
            Expr::col((a.clone(), annotations::Column::ParameterId)),
            Alias::new("pid"),
        )
        .expr_as(Expr::cust("COUNT(DISTINCT a.id)"), Alias::new("ann_count"))
        .expr_as(Expr::cust("COUNT(DISTINCT r.time)"), Alias::new("pts"))
        .from_as(annotations::Entity, a.clone())
        .join_as(
            JoinType::LeftJoin,
            readings::Entity,
            r_.clone(),
            Condition::all()
                .add(
                    Expr::col((r_.clone(), readings::Column::SiteId))
                        .equals((a.clone(), annotations::Column::SiteId)),
                )
                .add(
                    Expr::col((r_.clone(), readings::Column::ParameterId))
                        .equals((a.clone(), annotations::Column::ParameterId)),
                )
                .add(Expr::cust_with_values(
                    "r.time >= GREATEST(a.start_time, $1) AND r.time <= LEAST(a.end_time, $2)",
                    [start, end],
                ))
                .add(Expr::col((r_.clone(), readings::Column::WithdrawnAt)).is_null()),
        )
        .and_where(Expr::col((a.clone(), annotations::Column::SiteId)).eq(site.id))
        .and_where(Expr::col((a.clone(), annotations::Column::EndTime)).gte(start))
        .and_where(Expr::col((a.clone(), annotations::Column::StartTime)).lte(end))
        .add_group_by([Expr::col((a.clone(), annotations::Column::ParameterId))])
        .take()
        .build(PostgresQueryBuilder);
    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    for r in &rows {
        let counts = AnnotationCounts::from_query_result(r, "")?;
        let s = slot(&mut by_param, counts.pid);
        s.annotation_count = counts.ann_count;
        s.annotated_points = counts.pts;
    }

    // Flagged and extra-replicate rows in one pass over the range's readings.
    let (sql, values) = SeaQuery::select()
        .expr_as(Expr::col(readings::Column::ParameterId), Alias::new("pid"))
        .expr_as(
            Expr::cust("COUNT(*) FILTER (WHERE is_flagged = TRUE)"),
            Alias::new("flagged"),
        )
        .expr_as(
            Expr::cust("COUNT(*) FILTER (WHERE replicate_index > 0)"),
            Alias::new("reps"),
        )
        .from(readings::Entity)
        .and_where(Expr::col(readings::Column::SiteId).eq(site.id))
        .and_where(Expr::col(readings::Column::Time).gte(start))
        .and_where(Expr::col(readings::Column::Time).lte(end))
        .and_where(Expr::col(readings::Column::WithdrawnAt).is_null())
        .and_where(Expr::cust("(is_flagged = TRUE OR replicate_index > 0)"))
        .add_group_by([Expr::col(readings::Column::ParameterId)])
        .take()
        .build(PostgresQueryBuilder);
    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    for r in &rows {
        let counts = CurationCounts::from_query_result(r, "")?;
        let s = slot(&mut by_param, counts.pid);
        s.flagged_readings = counts.flagged;
        s.replicate_readings = counts.reps;
    }

    // Breaching readings, from the same definition `/sites/{id}/alarms` serves, so the count and
    // the export it gates cannot disagree. `alarm_events` is deliberately not the source: it holds
    // the sweeper's episodes, which exist only from when the sweeper first saw a slot, while an
    // export covers all of history.
    for (pid, n) in crate::routes::private::alarms::service::count_violations_by_parameter(
        &state.db,
        site.id,
        query.start,
        query.end,
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

// --- Statistics ---

/// Period statistics for a site's parameters over a range.
#[utoipa::path(
    get,
    path = "/api/sites/{site_id}/statistics",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
        StatisticsQuery
    ),
    responses(
        (status = 200, description = "Period statistics per parameter", body = StatisticsResponse),
        (status = 400, description = "Invalid query parameters"),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn get_site_statistics(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
    Query(query): Query<StatisticsQuery>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Response> {
    let (site, _project) = resolve_site_with_project(&state.db, &site_id).await?;
    require_site_in_scope(&scope, &site)?;

    let measurement_type = match query.measurement_type.as_deref() {
        None | Some("continuous") => "continuous",
        Some("spot") => "spot",
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "measurement_type must be 'continuous' or 'spot', not '{other}'"
            )));
        }
    };

    let effective_start = query.start.unwrap_or_else(|| {
        Utc::now() - chrono::Duration::days(state.config.default_readings_lookback_days)
    });
    validate_optional_time_range(Some(effective_start), query.end)?;

    let parameter_ids: Vec<Uuid> = match query.parameter_ids.as_deref() {
        Some(csv) => csv
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Uuid::parse_str)
            .collect::<Result<_, _>>()
            .map_err(|_| AppError::BadRequest("parameter_ids must be UUIDs".to_string()))?,
        None => {
            use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
            site_parameters::Entity::find()
                .filter(site_parameters::Column::SiteId.eq(site.id))
                .all(&state.db)
                .await?
                .into_iter()
                .map(|sp| sp.parameter_id)
                .collect()
        }
    };
    if parameter_ids.is_empty() {
        return Ok(Json(StatisticsResponse {
            site: SiteRef {
                id: site.id,
                name: site.name.clone(),
            },
            start: effective_start,
            end: query.end,
            measurement_type: measurement_type.to_string(),
            parameters: Vec::new(),
        })
        .into_response());
    }

    let p = Alias::new("p");
    let sp = Alias::new("sp");
    let v = Alias::new("v");
    let mut in_range = Condition::all()
        .add(
            Expr::col((v.clone(), readings::Column::ParameterId))
                .equals((p.clone(), parameters::Column::Id)),
        )
        .add(Expr::col((v.clone(), readings::Column::Time)).gte(effective_start));
    if let Some(end) = query.end {
        in_range = in_range.add(Expr::col((v.clone(), readings::Column::Time)).lte(end));
    }
    let agg = |sql: &str, name: &str| (Expr::cust(sql.to_string()), Alias::new(name.to_string()));
    let mut statistics = SeaQuery::select();
    statistics.expr_as(
        Expr::col((p.clone(), parameters::Column::Id)),
        Alias::new("parameter_id"),
    );
    statistics
        .column((p.clone(), parameters::Column::Code))
        .column((p.clone(), parameters::Column::Name))
        .expr_as(
            Expr::col((p.clone(), parameters::Column::DefaultUnits)),
            Alias::new("units"),
        )
        .column((sp.clone(), site_parameters::Column::DecimalPlaces));
    for (expr, name) in [
        agg("COUNT(v.time)", "time_points"),
        agg("COUNT(v.value)", "n"),
        agg(
            "PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY v.value)",
            "median",
        ),
        agg("AVG(v.value)", "mean"),
        agg("STDDEV_SAMP(v.value)", "stdev_sample"),
        agg("MIN(v.value)", "min_value"),
        agg("MAX(v.value)", "max_value"),
    ] {
        statistics.expr_as(expr, name);
    }
    let (sql, values) = statistics
        .from_as(parameters::Entity, p.clone())
        .join_as(
            JoinType::LeftJoin,
            site_parameters::Entity,
            sp.clone(),
            Condition::all()
                .add(
                    Expr::col((sp.clone(), site_parameters::Column::ParameterId))
                        .equals((p.clone(), parameters::Column::Id)),
                )
                .add(Expr::col((sp.clone(), site_parameters::Column::SiteId)).eq(site.id)),
        )
        .join_subquery(
            JoinType::LeftJoin,
            value_source(measurement_type, site.id, &parameter_ids),
            v.clone(),
            in_range,
        )
        .and_where(Expr::col((p.clone(), parameters::Column::Id)).is_in(parameter_ids.clone()))
        .add_group_by([
            Expr::col((p.clone(), parameters::Column::Id)),
            Expr::col((p.clone(), parameters::Column::Code)),
            Expr::col((p.clone(), parameters::Column::Name)),
            Expr::col(Alias::new("units")),
            Expr::col((sp.clone(), site_parameters::Column::DecimalPlaces)),
        ])
        .order_by((p.clone(), parameters::Column::Code), Order::Asc)
        .take()
        .build(PostgresQueryBuilder);

    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;

    let mut parameters = Vec::with_capacity(rows.len());
    for row in &rows {
        let r = StatisticsRow::from_query_result(row, "")?;
        parameters.push(ParameterStatistics {
            parameter_id: r.parameter_id,
            code: r.code,
            name: r.name,
            units: r.units,
            decimal_places: r.decimal_places,
            time_points: r.time_points,
            n: r.n,
            nulls: r.time_points - r.n,
            median: r.median,
            mean: r.mean,
            stdev_sample: r.stdev_sample,
            min: r.min_value,
            max: r.max_value,
        });
    }

    Ok(Json(StatisticsResponse {
        site: SiteRef {
            id: site.id,
            name: site.name.clone(),
        },
        start: effective_start,
        end: query.end,
        measurement_type: measurement_type.to_string(),
        parameters,
    })
    .into_response())
}

// --- Replicate export ---

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
    require_site_in_scope(&scope, &site)?;

    let effective_start = query.start.unwrap_or_else(|| {
        Utc::now() - chrono::Duration::days(state.config.default_readings_lookback_days)
    });
    validate_optional_time_range(Some(effective_start), query.end)?;

    let parameter_ids = parameter_id_filter(query.parameter_ids.as_deref())?;

    let r_ = Alias::new("r");
    let p = Alias::new("p");
    let ds = Alias::new("ds");
    let mut spot_replicates = Condition::all()
        .add(Expr::col((r_.clone(), readings::Column::SiteId)).eq(site.id))
        .add(Expr::col((r_.clone(), readings::Column::Time)).gte(effective_start))
        .add(Expr::col((r_.clone(), readings::Column::MeasurementType)).eq("spot"))
        .add(Expr::col((r_.clone(), readings::Column::SampleId)).is_not_null());
    if let Some(end) = query.end {
        spot_replicates =
            spot_replicates.add(Expr::col((r_.clone(), readings::Column::Time)).lte(end));
    }
    if let Some(ids) = parameter_ids {
        spot_replicates =
            spot_replicates.add(Expr::col((r_.clone(), readings::Column::ParameterId)).is_in(ids));
    }
    if !query.include_withdrawn.unwrap_or(false) {
        spot_replicates =
            spot_replicates.add(Expr::col((r_.clone(), readings::Column::WithdrawnAt)).is_null());
    }

    let (sql, values) = SeaQuery::select()
        .column((r_.clone(), readings::Column::Time))
        .expr_as(
            Expr::col((p.clone(), parameters::Column::Code)),
            Alias::new("parameter"),
        )
        .column((r_.clone(), readings::Column::SampleId))
        .column((r_.clone(), readings::Column::ReplicateIndex))
        .expr_as(served::continuous_value(), Alias::new("value"))
        .expr_as(
            Func::coalesce([
                Expr::col((r_.clone(), readings::Column::IsFlagged)),
                Expr::value(false),
            ]),
            Alias::new("flagged"),
        )
        .expr_as(
            Expr::col((r_.clone(), readings::Column::WithdrawnAt)).is_not_null(),
            Alias::new("withdrawn"),
        )
        .column((ds.clone(), data_streams::Column::SourceSystem))
        .column((ds.clone(), data_streams::Column::SourceKey))
        .from_as(readings::Entity, r_.clone())
        .join_as(
            JoinType::InnerJoin,
            parameters::Entity,
            p.clone(),
            Expr::col((p.clone(), parameters::Column::Id))
                .equals((r_.clone(), readings::Column::ParameterId)),
        )
        .join_as(
            JoinType::LeftJoin,
            data_streams::Entity,
            ds.clone(),
            Expr::col((ds.clone(), data_streams::Column::Id))
                .equals((r_.clone(), readings::Column::StreamId)),
        )
        .cond_where(spot_replicates)
        .order_by((r_.clone(), readings::Column::Time), Order::Asc)
        .order_by((p.clone(), parameters::Column::Code), Order::Asc)
        .order_by((r_.clone(), readings::Column::ReplicateIndex), Order::Asc)
        .take()
        .build(PostgresQueryBuilder);

    let rows: Vec<ReplicateRow> = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .iter()
        .filter_map(|row| ReplicateRow::from_query_result(row, "").ok())
        .collect();

    if query.format == "csv" {
        let mut writer = crate::common::csv::CsvWriter::new();
        writer.row([
            "time",
            "parameter",
            "sample_id",
            "replicate_index",
            "value",
            "flagged",
            "withdrawn",
            "source_system",
            "source_key",
        ]);
        for r in &rows {
            writer.row([
                r.time.with_timezone(&Utc).to_rfc3339(),
                r.parameter.clone(),
                r.sample_id.map(|id| id.to_string()).unwrap_or_default(),
                r.replicate_index.to_string(),
                r.value.map(|v| v.to_string()).unwrap_or_default(),
                r.flagged.to_string(),
                r.withdrawn.to_string(),
                r.source_system.clone().unwrap_or_default(),
                r.source_key.clone().unwrap_or_default(),
            ]);
        }
        let csv = writer.finish();
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

// --- Sensor versus grab ---

/// Sensor-vs-grab comparison for one parameter at a site.
///
/// Time-aligns each grab sample to the continuous sensor readings in a window after it and returns
/// the paired values plus their difference. Supports JSON (default) and CSV.
#[utoipa::path(
    get,
    path = "/api/sites/{site_id}/export/sensor-vs-grab",
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

    require_site_in_scope(&scope, &site)?;

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

    let r_ = Alias::new("r");
    let smp = Alias::new("smp");
    let g = Alias::new("g");
    // A grab instant is its sample's statistics, or the lone measurement itself: a single reading
    // forms no sample, and n = 1 is derived from it here.
    let grabs = SeaQuery::select()
        .column((r_.clone(), readings::Column::SiteId))
        .column((r_.clone(), readings::Column::ParameterId))
        .expr_as(
            Expr::col((r_.clone(), readings::Column::Time)),
            Alias::new("collected_at"),
        )
        .expr_as(
            Expr::cust("COALESCE(MAX(smp.mean), AVG(COALESCE(r.calibrated_value, r.raw_value)))"),
            Alias::new("mean"),
        )
        .expr_as(Expr::cust("MAX(smp.stdev)"), Alias::new("stdev"))
        .expr_as(
            Expr::cust("COALESCE(MAX(smp.n), COUNT(*)::int)"),
            Alias::new("n"),
        )
        .from_as(readings::Entity, r_.clone())
        .join_as(
            JoinType::LeftJoin,
            samples::Entity,
            smp.clone(),
            Expr::col((smp.clone(), samples::Column::Id))
                .equals((r_.clone(), readings::Column::SampleId)),
        )
        .and_where(Expr::col((r_.clone(), readings::Column::SiteId)).eq(site.id))
        .and_where(Expr::col((r_.clone(), readings::Column::ParameterId)).eq(query.parameter_id))
        .cond_where(served::served_spot_at(&r_))
        .add_group_by([
            Expr::col((r_.clone(), readings::Column::SiteId)),
            Expr::col((r_.clone(), readings::Column::ParameterId)),
            Expr::col((r_.clone(), readings::Column::Time)),
        ])
        .take();
    // Continuous = anything that is not a grab ('spot') or derived reading; `IS DISTINCT FROM`
    // keeps NULL-typed legacy/seed readings on the continuous side.
    let window = SeaQuery::select()
        .expr_as(
            Expr::cust("avg(COALESCE(r.calibrated_value, r.raw_value))"),
            Alias::new("sensor_avg"),
        )
        .expr_as(
            Expr::cust("stddev_samp(COALESCE(r.calibrated_value, r.raw_value))"),
            Alias::new("sensor_sd"),
        )
        .expr_as(Expr::cust("count(*)"), Alias::new("sensor_n"))
        .from_as(readings::Entity, r_.clone())
        .and_where(Expr::cust("r.site_id = g.site_id"))
        .and_where(Expr::cust("r.parameter_id = g.parameter_id"))
        .and_where(Expr::cust("r.measurement_type IS DISTINCT FROM 'spot'"))
        .and_where(Expr::cust("r.measurement_type IS DISTINCT FROM 'derived'"))
        .and_where(Expr::cust("r.is_flagged IS NOT TRUE"))
        .and_where(Expr::col((r_.clone(), readings::Column::ReplicateIndex)).eq(0))
        .and_where(Expr::cust_with_values(
            "r.time >= g.collected_at + ($1 * interval '1 hour')",
            [query.window_start_hours],
        ))
        .and_where(Expr::cust_with_values(
            "r.time <= g.collected_at + ($1 * interval '1 hour')",
            [query.window_end_hours],
        ))
        .take();
    let mut in_range = Condition::all()
        .add(Expr::col((g.clone(), Alias::new("collected_at"))).gte(effective_start));
    if let Some(end) = effective_end {
        in_range = in_range.add(Expr::col((g.clone(), Alias::new("collected_at"))).lte(end));
    }
    let (sql, values) = SeaQuery::select()
        .expr_as(
            Expr::col((g.clone(), Alias::new("collected_at"))),
            Alias::new("grab_time"),
        )
        .expr_as(
            Expr::col((g.clone(), Alias::new("mean"))),
            Alias::new("grab_value"),
        )
        .expr_as(
            Expr::col((g.clone(), Alias::new("stdev"))),
            Alias::new("grab_sd"),
        )
        .expr_as(
            Expr::col((g.clone(), Alias::new("n"))),
            Alias::new("grab_n"),
        )
        .expr(Expr::cust("agg.sensor_avg"))
        .expr(Expr::cust("agg.sensor_sd"))
        .expr(Expr::cust("agg.sensor_n"))
        .from_subquery(grabs, g.clone())
        .join_lateral(
            JoinType::LeftJoin,
            window,
            Alias::new("agg"),
            Condition::all().add(Expr::cust("true")),
        )
        .cond_where(in_range)
        .order_by((g.clone(), Alias::new("collected_at")), Order::Asc)
        .take()
        .build(PostgresQueryBuilder);

    let query_result = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
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
        let mut writer = crate::common::csv::CsvWriter::new();
        writer.row([
            "time",
            "grab_value",
            "grab_sd",
            "grab_n",
            "sensor_avg",
            "sensor_sd",
            "sensor_n",
            "difference",
        ]);
        for r in &rows {
            writer.row([
                r.time.to_rfc3339(),
                fmt(r.grab_value),
                fmt(r.grab_sd),
                r.grab_n.to_string(),
                fmt(r.sensor_avg),
                fmt(r.sensor_sd),
                r.sensor_n.to_string(),
                fmt(r.difference),
            ]);
        }
        let csv = writer.finish();
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

// --- Sensor identity bands ---

/// `GET /sites/{site_id}/sensor_identity`, deployment bands + calibration markers per parameter.
#[utoipa::path(
    get,
    path = "/api/sites/{site_id}/sensor_identity",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
        SensorIdentityQuery
    ),
    responses(
        (status = 200, description = "Identity bands + calibration markers", body = SensorIdentityResponse),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn get_site_sensor_identity(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
    Query(query): Query<SensorIdentityQuery>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<SensorIdentityResponse>> {
    let db = &state.db;
    let site = resolve_site(db, &site_id).await?;
    require_site_in_scope(&scope, &site)?;
    validate_time_range(query.start, query.end)?;

    let param_filter = query.parameter_ids.as_deref().map(parse_uuid_csv);

    let (band_sql, band_values) =
        sensor_identity_bands_query(site.id, query.start, query.end, param_filter.as_deref())
            .build(PostgresQueryBuilder);
    let band_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            band_sql,
            band_values,
        ))
        .await?;

    let mut bands: HashMap<Uuid, Vec<IdentityBand>> = HashMap::new();
    for row in &band_rows {
        let row = BandRow::from_query_result(row, "")?;
        bands
            .entry(row.parameter_id)
            .or_default()
            .push(IdentityBand {
                deployment_id: row.deployment_id,
                sensor_id: row.sensor_id,
                sensor_serial: row.sensor_serial,
                sensor_name: row.sensor_name,
                site_id: row.site_id,
                site_name: Some(site.name.clone()),
                parameter_id: row.parameter_id,
                from: row.deployed_from.with_timezone(&Utc),
                until: row.deployed_until.map(|u| u.with_timezone(&Utc)),
            });
    }

    // Calibration markers: calibrations (overlapping the window) of the sensors deployed at this
    // site over the window, grouped by the sensor's parameter.
    // Markers use the calibration's own parameter (a sensor carries none), so a
    // calibration whose parameter is not resolved yet has no series to sit on and is not a
    // plottable marker. Lab curves cannot appear here at all: they live in `standard_curves`,
    // which has no window to plot.
    let (cal_sql, cal_values) =
        sensor_calibration_markers_query(site.id, query.start, query.end, param_filter.as_deref())
            .build(PostgresQueryBuilder);
    let cal_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            cal_sql,
            cal_values,
        ))
        .await?;

    let mut calibrations: HashMap<Uuid, Vec<CalibrationMarker>> = HashMap::new();
    for row in &cal_rows {
        let row = MarkerRow::from_query_result(row, "")?;
        calibrations
            .entry(row.parameter_id)
            .or_default()
            .push(CalibrationMarker {
                calibration_id: row.calibration_id,
                sensor_id: row.sensor_id,
                slope: row.slope,
                intercept: row.intercept,
                valid_from: row.valid_from.with_timezone(&Utc),
                valid_until: row.valid_until.map(|u| u.with_timezone(&Utc)),
            });
    }

    Ok(Json(SensorIdentityResponse {
        site_id: site.id,
        bands,
        calibrations,
    }))
}

// --- Router ---

pub fn service_router(state: &AppState) -> OpenApiRouter {
    // Sites are field metadata: RIVER members (and write_metadata tokens) may create/edit them.
    let crud = Site::router(&state.db).layer(middleware::from_fn(require_crud(
        Capability::ReadMetadata,
        Capability::WriteFieldMetadata,
        TokenAccess::Same,
    )));

    // Mounted by explicit path, nest-relative, while each handler's `#[utoipa::path]` declares
    // the absolute URL: the spec is assembled from those declarations, not from this router.
    let data = OpenApiRouter::new()
        .route("/{site_id}/readings", get(get_site_readings))
        .route(
            "/{site_id}/aggregates/{resolution}",
            get(get_site_aggregates),
        )
        .route("/{site_id}/status_events", get(get_site_status_events))
        .route(
            "/{site_id}/alarms",
            get(crate::routes::private::alarms::views::get_site_alarms),
        )
        .route("/{site_id}/annotations", get(get_site_annotations))
        .route("/{site_id}/export/summary", get(get_site_export_summary))
        .route("/{site_id}/export/replicates", get(get_site_replicates))
        .route("/{site_id}/export/sensor-vs-grab", get(get_sensor_vs_grab))
        .route("/{site_id}/statistics", get(get_site_statistics))
        .route("/{site_id}/sensor_identity", get(get_site_sensor_identity))
        .route(
            "/{site_id}/last_curve",
            get(crate::routes::private::standard_curves::views::last_used_curve),
        )
        .with_state(state.clone())
        .layer(middleware::from_fn(require_read_data));

    let metadata = OpenApiRouter::new()
        .route("/{site_id}/parameters", get(list_site_parameters))
        .route("/{site_id}/detail", get(get_site_detail))
        .with_state(state.clone())
        .layer(middleware::from_fn(require_read_metadata));

    crud.merge(data).merge(metadata)
}
