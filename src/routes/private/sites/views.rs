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
    SelectStatement, UnionType,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, ExprTrait, FromQueryResult, Order, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, Statement,
};
use utoipa_axum::router::OpenApiRouter;
use uuid::Uuid;

use super::models::*;
use super::service::*;
use crate::common::authz::{Capability, TokenAccess};
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

    // Enforce project scope
    if !scope.allows_project_opt(site.project_id) {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }

    let params_list = site_parameters::Entity::find()
        .filter(site_parameters::Column::SiteId.eq(site.id))
        .filter(site_parameters::Column::IsActive.eq(true))
        .order_by_asc(site_parameters::Column::Name)
        .all(&state.db)
        .await?;

    let param_ids: Vec<Uuid> = params_list.iter().map(|p| p.parameter_id).collect();
    let globals = site_parameters::catalog_map(&state.db, param_ids.iter().copied()).await?;
    let extents = parameter_extents(&state.db, site.id).await?;
    let declared = declared_frequencies(&state.db, site.id).await?;

    let response: Vec<ParameterResponse> = params_list
        .into_iter()
        .map(|p| build_parameter_response(p, &globals, &extents, &declared))
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

    // Enforce project scope
    if !scope.allows_project_opt(site.project_id) {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }

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
    let declared = declared_frequencies(&state.db, site.id).await?;

    let parameters: Vec<ParameterResponse> = params_list
        .into_iter()
        .map(|p| build_parameter_response(p, &globals, &extents, &declared))
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
    // Replicates and statistics never share one file (Q32): one is a row per replicate, the other
    // a row per instant, and a file carrying both is neither. Asking for both was silently served
    // as replicates alone, which reads as "this window has no sample statistics".
    if include_replicates && query.include_sample_stats.unwrap_or(false) {
        return Err(AppError::BadRequest(
            "include_sample_stats and include_replicates cannot be combined: the replicate rows \
             and the per-instant statistics are separate files. Request the statistics here, and \
             the replicates as their own download."
                .to_string(),
        ));
    }
    let annotations = Annotations {
        alarms: query.alarms.unwrap_or(false),
        flagged: query.include_flagged.unwrap_or(true),
        measurement_type: query.include_measurement_type.unwrap_or(false),
        sample_stats: query.include_sample_stats.unwrap_or(false),
        curves: query.include_curves.unwrap_or(false),
        origin: query.include_origin.unwrap_or(false),
        withdrawn: query.include_withdrawn.unwrap_or(false),
    };
    let include_flags = query.include_flags.unwrap_or(false);

    if let Some(mt) = query.measurement_type.as_deref()
        && !mt.is_empty()
    {
        crate::routes::private::readings::service::validate_measurement_type(Some(mt))?;
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
            |(times, params)| readings_table(times, params, include_flags, None),
            |(times, parameters)| async move {
                Ok(Json(ReadingsResponse {
                    project: project_ref,
                    site: site_ref,
                    start: None,
                    end: None,
                    times,
                    replicate_indices: None,
                    parameters,
                })
                .into_response())
            },
        )
        .await;
    }

    let num_params = params_list.len();

    let r_ = served::r();
    let t = Alias::new("t");
    let sv = Alias::new("sv");

    // Severity comes from the one shared ladder (alarms engine). NULL when the slot has no
    // threshold at any tier (no `t` row); otherwise the ladder treats all-NULL bounds as 0
    // (disabled).
    let severity_expr = |value_expr: &str| -> Expr {
        if annotations.alarms {
            let sev = crate::routes::private::alarms::service::severity_case(
                value_expr,
                "t.warning_min",
                "t.warning_max",
                "t.alarm_min",
                "t.alarm_max",
            );
            Expr::cust(format!(
                "CASE WHEN t.parameter_id IS NULL THEN NULL ELSE ({sev})::smallint END"
            ))
        } else {
            Expr::cust("NULL::smallint")
        }
    };
    // The 3-tier threshold per slot via the single engine definition (site → global → parameter
    // default), scoped to this site and LEFT JOINed, so a parameter with only defaults still gets
    // a severity (the old direct join to alarm_thresholds did not).
    let thresholds = || {
        crate::routes::private::alarms::service::resolve_thresholds_query(
            Some(site.id),
            Some(param_ids.to_vec()),
        )
    };

    let in_window = |alias: &Alias| {
        let mut cond = Condition::all()
            .add(Expr::col((alias.clone(), readings::Column::Time)).gte(effective_start));
        if let Some(end) = effective_end {
            cond = cond.add(Expr::col((alias.clone(), readings::Column::Time)).lte(end));
        }
        cond
    };
    let slot = |alias: &Alias| {
        let mut cond = Condition::all()
            .add(Expr::col((alias.clone(), readings::Column::SiteId)).eq(site.id))
            .add(
                Expr::col((alias.clone(), readings::Column::ParameterId)).is_in(param_ids.to_vec()),
            )
            .add(in_window(alias));
        if !annotations.flagged {
            cond = cond.add(Expr::cust("(r.is_flagged IS NOT TRUE)"));
        }
        if let Some(sid) = query.sample_id {
            cond = cond.add(Expr::col((alias.clone(), readings::Column::SampleId)).eq(sid));
        }
        cond
    };

    let query_statement = if include_replicates {
        // Every stored row, one per replicate; the caller reconstructs the groups.
        // "continuous" means everything that is not a grab: derived rows plot on the continuous
        // line (matching the continuous aggregates, which exclude only 'spot'), and legacy NULL
        // rows predate the measurement_type column.
        let mut rows = slot(&r_);
        match measurement_type_filter {
            "" => {}
            "continuous" => {
                rows = rows.add(Expr::cust("(r.measurement_type IS DISTINCT FROM 'spot')"));
            }
            other => {
                rows = rows.add(
                    Expr::col((r_.clone(), readings::Column::MeasurementType))
                        .eq(other.to_string()),
                );
            }
        }
        // The collapsed spot arm excludes withdrawn rows; the replicate view was exporting them
        // as ordinary values, which publishes a number the source has taken back.
        if !query.include_withdrawn.unwrap_or(false) {
            rows = rows.add(Expr::col((r_.clone(), readings::Column::WithdrawnAt)).is_null());
        }
        let mut replicates = SeaQuery::select();
        replicates
            .column((r_.clone(), readings::Column::ParameterId))
            .column((r_.clone(), readings::Column::Time))
            .column((r_.clone(), readings::Column::ReplicateIndex))
            .expr_as(served::continuous_value(), Alias::new("value"))
            .expr_as(
                severity_expr("COALESCE(r.calibrated_value, r.raw_value)"),
                Alias::new("severity"),
            )
            .column((r_.clone(), readings::Column::IsFlagged))
            .column((r_.clone(), readings::Column::FlagReason))
            .column((r_.clone(), readings::Column::MeasurementType))
            .column((r_.clone(), readings::Column::Unverified))
            .column((r_.clone(), readings::Column::SampleId))
            .column((r_.clone(), readings::Column::CalibrationId))
            .column((r_.clone(), readings::Column::StandardCurveId))
            .expr_as(
                Expr::col((r_.clone(), readings::Column::WithdrawnAt)).is_not_null(),
                Alias::new("withdrawn"),
            )
            .from_as(readings::Entity, r_.clone());
        if annotations.alarms {
            replicates.join_subquery(
                JoinType::LeftJoin,
                thresholds(),
                t.clone(),
                Condition::all()
                    .add(
                        Expr::col((t.clone(), Alias::new("parameter_id")))
                            .equals((r_.clone(), readings::Column::ParameterId)),
                    )
                    .add(
                        Expr::col((t.clone(), Alias::new("site_id")))
                            .equals((r_.clone(), readings::Column::SiteId)),
                    ),
            );
        }
        replicates
            .cond_where(rows)
            .order_by((r_.clone(), readings::Column::ParameterId), Order::Asc)
            .order_by((r_.clone(), readings::Column::Time), Order::Asc)
            .order_by((r_.clone(), readings::Column::ReplicateIndex), Order::Asc);
        replicates.take()
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
                "" => (true, true, None),
                "continuous" => (true, false, None),
                "spot" => (false, true, None),
                other => (true, false, Some(other.to_string())),
            };
        let base_cols = |q: &mut SelectStatement| {
            q.column((r_.clone(), readings::Column::ParameterId))
                .column((r_.clone(), readings::Column::Time))
                .column((r_.clone(), readings::Column::SiteId))
                .column((r_.clone(), readings::Column::IsFlagged))
                .column((r_.clone(), readings::Column::FlagReason))
                .column((r_.clone(), readings::Column::MeasurementType))
                .column((r_.clone(), readings::Column::SampleId))
                .column((r_.clone(), readings::Column::CalibrationId))
                .column((r_.clone(), readings::Column::StandardCurveId))
                .expr_as(
                    Expr::col((r_.clone(), readings::Column::WithdrawnAt)).is_not_null(),
                    Alias::new("withdrawn"),
                )
                .column((r_.clone(), readings::Column::Unverified));
        };
        let served_cols = [
            Alias::new("value"),
            Alias::new("parameter_id"),
            Alias::new("time"),
            Alias::new("site_id"),
            Alias::new("is_flagged"),
            Alias::new("flag_reason"),
            Alias::new("measurement_type"),
            Alias::new("sample_id"),
            Alias::new("calibration_id"),
            Alias::new("standard_curve_id"),
            Alias::new("withdrawn"),
            Alias::new("unverified"),
        ];
        let mut arms: Vec<SelectStatement> = Vec::new();
        if include_continuous_arm {
            let mut cond = slot(&r_).add(served::continuous_rows());
            if let Some(other) = &continuous_extra {
                cond = cond.add(
                    Expr::col((r_.clone(), readings::Column::MeasurementType)).eq(other.clone()),
                );
            }
            let mut arm = SeaQuery::select();
            arm.expr_as(served::continuous_value(), Alias::new("value"));
            base_cols(&mut arm);
            arm.from_as(readings::Entity, r_.clone()).cond_where(cond);
            arms.push(arm.take());
        }
        if include_spot_arm {
            // A retracted instant is served only when asked for, and then the ordering below
            // prefers a live replicate, so `withdrawn` on the served row means the whole group is
            // retracted.
            let mut cond = slot(&r_)
                .add(Expr::col((r_.clone(), readings::Column::MeasurementType)).eq("spot"));
            if !annotations.withdrawn {
                cond = cond.add(Expr::col((r_.clone(), readings::Column::WithdrawnAt)).is_null());
            }
            // One row per slot instant, not per stream; the key and its ordering are
            // `common::served`, shared with the public arm and the alarm evaluator.
            let smp = Alias::new("smp");
            let mut group = SeaQuery::select();
            group
                .distinct_on(served::spot_instant_key())
                .expr_as(served::spot_value(), Alias::new("value"));
            base_cols(&mut group);
            group
                .from_as(readings::Entity, r_.clone())
                .join_as(
                    JoinType::LeftJoin,
                    samples::Entity,
                    smp.clone(),
                    Expr::col((smp, samples::Column::Id))
                        .equals((r_.clone(), readings::Column::SampleId)),
                )
                .cond_where(cond);
            for (expr, order) in served::spot_instant_order() {
                group.order_by_expr(expr, order);
            }
            let sp = Alias::new("sp");
            arms.push(
                SeaQuery::select()
                    .columns(served_cols.map(|c| (sp.clone(), c)))
                    .from_subquery(group.take(), sp.clone())
                    .take(),
            );
        }
        let mut arms = arms.into_iter();
        let first = arms.next().unwrap_or_default();
        let inner = arms.fold(first, |mut acc, arm| acc.union(UnionType::All, arm).take());

        let mut series = SeaQuery::select();
        series
            .column((sv.clone(), Alias::new("parameter_id")))
            .column((sv.clone(), Alias::new("time")))
            .expr_as(Expr::cust("NULL::smallint"), Alias::new("replicate_index"))
            .column((sv.clone(), Alias::new("value")))
            .expr_as(severity_expr("sv.value"), Alias::new("severity"))
            .column((sv.clone(), Alias::new("is_flagged")))
            .column((sv.clone(), Alias::new("flag_reason")))
            .column((sv.clone(), Alias::new("measurement_type")))
            .column((sv.clone(), Alias::new("sample_id")))
            .column((sv.clone(), Alias::new("calibration_id")))
            .column((sv.clone(), Alias::new("standard_curve_id")))
            .column((sv.clone(), Alias::new("withdrawn")))
            .column((sv.clone(), Alias::new("unverified")))
            .from_subquery(inner, sv.clone());
        if annotations.alarms {
            series.join_subquery(
                JoinType::LeftJoin,
                thresholds(),
                t.clone(),
                Condition::all()
                    .add(
                        Expr::col((t.clone(), Alias::new("parameter_id")))
                            .equals((sv.clone(), Alias::new("parameter_id"))),
                    )
                    .add(
                        Expr::col((t.clone(), Alias::new("site_id")))
                            .equals((sv.clone(), Alias::new("site_id"))),
                    ),
            );
        }
        series
            .order_by((sv.clone(), Alias::new("parameter_id")), Order::Asc)
            .order_by((sv.clone(), Alias::new("time")), Order::Asc);
        series.take()
    };
    let (sql, values) = query_statement.build(PostgresQueryBuilder);

    let query_result = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
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

    // One derivation of the row axis, and the row-to-column mapping that goes with it. Replicates
    // share timestamps, so that view's axis is `(time, replicate_index)` pairs; every other view is
    // keyed by time alone.
    let mut replicate_keys: Vec<(DateTime<Utc>, i16)> = Vec::new();
    let times: Vec<DateTime<Utc>> = if include_replicates {
        // The union of every parameter's `(time, replicate_index)` pairs, sorted. Taking the
        // longest parameter's timestamps and filling by position dated one parameter's values to
        // another parameter's instants whenever the two had different row counts.
        let mut set: HashSet<(DateTime<Utc>, i16)> = HashSet::with_capacity(estimated_times);
        for rows in param_rows.values() {
            for r in rows {
                set.insert((r.time.with_timezone(&Utc), r.replicate_index.unwrap_or(0)));
            }
        }
        let mut keys: Vec<(DateTime<Utc>, i16)> = set.into_iter().collect();
        keys.sort_unstable();
        replicate_keys = keys;
        replicate_keys.iter().map(|(t, _)| *t).collect()
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
    let replicate_index_map: HashMap<(DateTime<Utc>, i16), usize> = replicate_keys
        .iter()
        .enumerate()
        .map(|(i, k)| (*k, i))
        .collect();
    let index = if include_replicates {
        RowIndex::ByReplicate(&replicate_index_map)
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

    // A retracted visit is a fact about the window whether or not its points are drawn, so the
    // count is served whenever the request covers the spot arm. Only instants no live replicate
    // survives on are counted: one retracted replicate is not a retracted visit.
    let withdrawn_counts: Option<HashMap<Uuid, i64>> =
        if include_replicates || measurement_type_filter == "continuous" {
            None
        } else {
            Some(
                count_withdrawn_instants(
                    &state.db,
                    site.id,
                    &params_list
                        .iter()
                        .map(|sp| sp.parameter_id)
                        .collect::<Vec<_>>(),
                    effective_start,
                    effective_end,
                )
                .await?,
            )
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
            let mut withdrawn = annotations.withdrawn.then(|| vec![None; len]);
            let mut unverified = vec![None; len];

            if let Some(rows) = param_rows.get(&sp.parameter_id) {
                for row in rows {
                    let Some(i) = index.of(row.time.with_timezone(&Utc), row.replicate_index)
                    else {
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
                    if let Some(v) = withdrawn.as_mut() {
                        v[i] = row.withdrawn;
                    }
                    unverified[i] = row.unverified;
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
                decimal_places: descriptor.decimal_places,
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
                withdrawn,
                unverified: Some(unverified),
                withdrawn_count: withdrawn_counts
                    .as_ref()
                    .map(|counts| counts.get(&sp.parameter_id).copied().unwrap_or(0)),
            }
        })
        .collect();

    let actual_start = times.first().copied();
    let actual_end = times.last().copied();

    let indices: Option<Vec<i16>> =
        include_replicates.then(|| replicate_keys.iter().map(|(_, i)| *i).collect());
    let table_indices = indices.clone();
    series::respond(
        &format,
        (times, param_data),
        move |(times, params)| {
            readings_table(times, params, include_flags, table_indices.as_deref())
        },
        |(times, parameters)| async move {
            let response = ReadingsResponse {
                project: project_ref,
                site: site_ref,
                start: actual_start,
                end: actual_end,
                times,
                replicate_indices: indices,
                parameters,
            };
            cache::cache_and_respond(&state, cache_key, &response, actual_end).await
        },
    )
    .await
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
    use crate::routes::private::alarms::models as alarm_models;
    use crate::routes::private::alarms::service as alarm_engine;
    let threshold_map: HashMap<Uuid, alarm_models::ResolvedThreshold> = if include_alarms {
        let (sql, values) =
            alarm_engine::resolve_thresholds_query(Some(site.id), Some(param_ids.clone()))
                .build(sea_orm::sea_query::PostgresQueryBuilder);
        let mut map = HashMap::new();
        for row in state
            .db
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
        map
    } else {
        HashMap::new()
    };

    // $1 = site_id, $2 = the parameter ids as one array, $3 start, $4 end.
    let base_values: Vec<sea_orm::Value> = vec![site.id.into(), param_ids.to_vec().into()];
    let start_param = 3;
    let end_param = 4;

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
          AND parameter_id = ANY($2)
          AND bucket >= ${start_param}
          AND bucket <= ${end_param}
        GROUP BY bucket, parameter_id{sensor_group}
        ORDER BY bucket ASC, parameter_id ASC{sensor_group}
        ",
        view = rollup.view(),
    );

    let rows: Vec<AggregateRow> = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            bind(&window),
        ))
        .await?
        .into_iter()
        .filter_map(|row| AggregateRow::from_query_result(&row, "").ok())
        .collect();

    let mut flagged = SeaQuery::select();
    flagged.expr_as(
        Expr::cust_with_values(
            "time_bucket($1::interval, time)",
            [bucket_interval(rollup).to_string()],
        ),
        Alias::new("bucket"),
    );
    flagged.column(readings::Column::ParameterId);
    if split {
        flagged.column(readings::Column::SensorId);
    } else {
        flagged.expr_as(Expr::cust("NULL::uuid"), Alias::new("sensor_id"));
    }
    flagged
        .expr_as(Expr::cust("COUNT(*)::bigint"), Alias::new("flagged_count"))
        .from(readings::Entity)
        .and_where(Expr::col(readings::Column::SiteId).eq(site.id))
        .and_where(Expr::col(readings::Column::ParameterId).is_in(param_ids.to_vec()))
        .and_where(Expr::col(readings::Column::Time).gte(query.start))
        .and_where(Expr::col(readings::Column::Time).lte(query.end))
        .and_where(Expr::col(readings::Column::IsFlagged).eq(true))
        .and_where(Expr::col(readings::Column::ReplicateIndex).eq(0))
        .and_where(Expr::cust("measurement_type IS DISTINCT FROM 'spot'"))
        .add_group_by([
            Expr::col(Alias::new("bucket")).into(),
            Expr::col(readings::Column::ParameterId).into(),
        ]);
    if split {
        flagged.add_group_by([Expr::col(readings::Column::SensorId).into()]);
    }
    let (flagged_sql, flagged_values) = flagged.take().build(PostgresQueryBuilder);

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

    // Enforce project scope
    if !scope.allows_project_opt(site.project_id) {
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
        rows = rows.limit(window.limit as u64).offset(window.offset as u64);
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
    if !scope.allows_project_opt(site.project_id) {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }
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
        .add_group_by([Expr::col((a.clone(), annotations::Column::ParameterId)).into()])
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
        .add_group_by([Expr::col(readings::Column::ParameterId).into()])
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
    if !scope.allows_project_opt(site.project_id) {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }

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
            Func::coalesce([
                Expr::col((sp.clone(), site_parameters::Column::DisplayUnits)),
                Expr::col((p.clone(), parameters::Column::DefaultUnits)),
            ]),
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
        agg("STDDEV_POP(v.value)", "stdev_population"),
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
            Expr::col((p.clone(), parameters::Column::Id)).into(),
            Expr::col((p.clone(), parameters::Column::Code)).into(),
            Expr::col((p.clone(), parameters::Column::Name)).into(),
            Expr::col(Alias::new("units")).into(),
            Expr::col((sp.clone(), site_parameters::Column::DecimalPlaces)).into(),
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
            stdev_population: r.stdev_population,
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
            Expr::col((r_.clone(), readings::Column::SiteId)).into(),
            Expr::col((r_.clone(), readings::Column::ParameterId)).into(),
            Expr::col((r_.clone(), readings::Column::Time)).into(),
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
    if !scope.allows_project_opt(site.project_id) {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }
    validate_time_range(query.start, query.end)?;

    let param_filter = query.parameter_ids.as_deref().map(parse_uuid_csv);

    // $1 = site_id, $2 = end, $3 = start, then optional parameter_ids.
    let mut band_sql = String::from(
        r"SELECT d.id AS deployment_id, d.sensor_id, s.serial_number AS sensor_serial,
                 s.name AS sensor_name, d.site_id, d.parameter_id, d.deployed_from, d.deployed_until
          FROM sensor_deployments d
          JOIN sensors s ON s.id = d.sensor_id
          WHERE d.site_id = $1
            AND d.deployed_from < $2
            AND COALESCE(d.deployed_until, 'infinity'::timestamptz) > $3",
    );
    let mut values: Vec<sea_orm::Value> =
        vec![site.id.into(), query.end.into(), query.start.into()];
    if let Some(ref pids) = param_filter
        && !pids.is_empty()
    {
        band_sql.push_str(" AND d.parameter_id = ANY($4)");
        values.push(pids.clone().into());
    }
    band_sql.push_str(" ORDER BY d.parameter_id, d.deployed_from");

    let band_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &band_sql,
            values.clone(),
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
    // Markers use the calibration's own parameter (the sensor no longer carries one), so a
    // calibration whose parameter is not resolved yet has no series to sit on and is not a
    // plottable marker. Lab curves cannot appear here at all: they live in `standard_curves`,
    // which has no window to plot.
    let mut cal_sql = String::from(
        r"SELECT c.id AS calibration_id, c.sensor_id, c.parameter_id,
                 c.slope, c.intercept, c.valid_from, c.valid_until
          FROM sensor_calibrations c
          WHERE c.sensor_id IN (
              SELECT DISTINCT d.sensor_id FROM sensor_deployments d
              WHERE d.site_id = $1
                AND d.deployed_from < $2
                AND COALESCE(d.deployed_until, 'infinity'::timestamptz) > $3",
    );
    if let Some(ref pids) = param_filter
        && !pids.is_empty()
    {
        cal_sql.push_str(" AND d.parameter_id = ANY($4)");
    }
    cal_sql.push_str(
        r" )
            AND c.parameter_id IS NOT NULL
            AND c.valid_from < $2
            AND COALESCE(c.valid_until, 'infinity'::timestamptz) > $3
          ORDER BY c.parameter_id, c.valid_from",
    );

    let cal_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &cal_sql,
            values,
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
