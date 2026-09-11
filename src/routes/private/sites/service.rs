//! The queries and resolution logic behind the site-scoped endpoints.

use std::collections::HashMap;

use axum::http::header::{self, HeaderValue};
use axum::response::Response;
use chrono::{DateTime, Utc};
use sea_orm::sea_query::{
    Alias, Expr, ExprTrait, Func, JoinType, PostgresQueryBuilder, Query as SeaQuery,
    SelectStatement,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, QueryOrder,
    QuerySelect, Statement,
};
use uuid::Uuid;

use super::models::*;
use crate::common::aggregates::Resolution;
use crate::common::series::{Cells, Table};
use crate::error::{AppError, AppResult};
use crate::routes::private::readings::models as readings;
use crate::routes::private::readings::samples;
use crate::routes::private::site_parameters;

// --- Site detail and the parameter list ---

pub(super) struct ParameterExtent {
    pub(super) data_start: Option<DateTime<Utc>>,
    pub(super) data_end: Option<DateTime<Utc>>,
    pub(super) reading_count: i64,
    pub(super) spot_count: i64,
    pub(super) continuous_count: i64,
}

pub(super) fn min_opt(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(Ord::min(a, b)),
        (some, None) | (None, some) => some,
    }
}

pub(super) fn max_opt(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(Ord::max(a, b)),
        (some, None) | (None, some) => some,
    }
}

/// The FILTER aggregates the extent passes count with. Text, because `COUNT(*) FILTER (WHERE ...)`
/// has no builder form; the statements they sit in are composed.
const SERVED_SPOT_COUNT: &str =
    "COUNT(*) FILTER (WHERE is_flagged IS NOT TRUE AND withdrawn_at IS NULL)::bigint";
const SPOT_COUNT: &str = "COUNT(*) FILTER (WHERE measurement_type = 'spot' \
     AND is_flagged IS NOT TRUE AND withdrawn_at IS NULL)";
const CONTINUOUS_COUNT: &str = "COUNT(*) FILTER (WHERE measurement_type IS DISTINCT FROM 'spot' \
     AND replicate_index = 0 AND is_flagged IS NOT TRUE)";

/// How far back the raw-readings freshness pass looks. Wide enough to cover the hourly
/// aggregate's refresh lag (one bucket + one schedule interval) many times over, narrow enough
/// that chunk exclusion keeps the scan to a handful of chunks.
pub(super) const RECENT_EXTENT_DAYS: i64 = 14;

/// Per-parameter data extents and cadence counts, assembled from the maintained summaries
/// instead of a full hypertable scan: an unbounded `readings` query pays a planning cost
/// proportional to the chunk count on every call, which is what made this endpoint the slow
/// half of a site page load.
///
/// - Continuous extents and counts come from `readings_hourly`, whose population (replicate 0,
///   unflagged, non-spot) is exactly what `continuous_count` mirrors.
/// - Spot extents and replicate counts come from the spot readings themselves, over
///   `idx_readings_spot_site_param_time`: a single measurement forms no `samples` row, so the
///   materialised groups do not speak for every grab.
/// - A bounded raw pass over the last `RECENT_EXTENT_DAYS` covers rows the hourly aggregate has
///   not refreshed yet and decides `has_*` for brand-new slots; it carries no flag filter, so a
///   freshly flagged tail still extends the extent.
/// - `data_streams.last_data_time` supplies a flag-agnostic newest instant per slot, so a series
///   whose trailing rows are all flagged and older than the raw pass still reports its true end.
///   The extents seed the chart range slider, and flagged points are drawn and exported, so the
///   flagged tail must stay inside the range.
/// - A probe over flagged rows alone supplies the same guarantee at the head, where there is no
///   cursor to speak for it. Flagged rows are the only population the summaries leave out, so
///   this is the whole of the correction; `idx_readings_flagged_site_param_time` is what keeps it
///   off an unbounded scan.
///
/// `data_end` from the aggregate alone is the last bucket start plus one bucket, which can
/// overstate by up to an hour; the exact sources win whenever they are newer. `reading_count`
/// counts the summarised populations (unflagged continuous plus spot replicates), not raw rows.
pub(super) async fn parameter_extents(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
) -> AppResult<HashMap<Uuid, ParameterExtent>> {
    let continuous = ContinuousExtentRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT parameter_id, MIN(bucket) AS min_bucket, MAX(bucket) AS max_bucket, \
                COALESCE(SUM(count), 0)::bigint AS count \
         FROM readings_hourly WHERE site_id = $1 AND parameter_id IS NOT NULL \
         GROUP BY parameter_id",
        [site_id.into()],
    ))
    .all(db)
    .await?;

    let spot = readings::Entity::find()
        .select_only()
        .column(readings::Column::ParameterId)
        .expr_as(Func::min(Expr::col(readings::Column::Time)), "min_time")
        .expr_as(Func::max(Expr::col(readings::Column::Time)), "max_time")
        .expr_as(Expr::cust(SERVED_SPOT_COUNT), "count")
        .filter(readings::Column::SiteId.eq(site_id))
        .filter(readings::Column::ParameterId.is_not_null())
        .filter(readings::Column::MeasurementType.eq("spot"))
        .group_by(readings::Column::ParameterId)
        .into_model::<SpotExtentRow>()
        .all(db)
        .await?;

    let recent = readings::Entity::find()
        .select_only()
        .column(readings::Column::ParameterId)
        .expr_as(Func::min(Expr::col(readings::Column::Time)), "min_time")
        .expr_as(Func::max(Expr::col(readings::Column::Time)), "max_time")
        .expr_as(Expr::cust(SPOT_COUNT), "spot_count")
        .expr_as(Expr::cust(CONTINUOUS_COUNT), "continuous_count")
        .filter(readings::Column::SiteId.eq(site_id))
        .filter(readings::Column::ParameterId.is_not_null())
        // An interval literal has no builder form, and keeping it a literal is what lets chunk
        // exclusion see the bound.
        .filter(Expr::cust(format!(
            "time > now() - INTERVAL '{RECENT_EXTENT_DAYS} days'"
        )))
        .group_by(readings::Column::ParameterId)
        .into_model::<RecentExtentRow>()
        .all(db)
        .await?;

    let mut flagged_heads: HashMap<Uuid, DateTime<Utc>> = HashMap::new();
    for row in readings::Entity::find()
        .select_only()
        .column(readings::Column::ParameterId)
        .expr_as(Func::min(Expr::col(readings::Column::Time)), "min_time")
        .filter(readings::Column::SiteId.eq(site_id))
        .filter(readings::Column::ParameterId.is_not_null())
        .filter(Expr::col(readings::Column::IsFlagged))
        .group_by(readings::Column::ParameterId)
        .into_model::<FlaggedHeadRow>()
        .all(db)
        .await?
    {
        if let Some(t) = row.min_time {
            flagged_heads.insert(row.parameter_id, t);
        }
    }

    let mut cursors: HashMap<Uuid, DateTime<Utc>> = HashMap::new();
    for row in CursorRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT sp.parameter_id, MAX(ds.last_data_time) AS max_time \
             FROM data_streams ds JOIN site_parameters sp ON ds.site_parameter_id = sp.id \
             WHERE sp.site_id = $1 AND ds.last_data_time IS NOT NULL \
             GROUP BY sp.parameter_id",
        [site_id.into()],
    ))
    .all(db)
    .await?
    {
        if let Some(t) = row.max_time {
            cursors.insert(row.parameter_id, t);
        }
    }

    let mut extents: HashMap<Uuid, ParameterExtent> = HashMap::new();
    let mut last_buckets: HashMap<Uuid, DateTime<Utc>> = HashMap::new();
    for r in continuous {
        let e = extents.entry(r.parameter_id).or_insert_with(empty_extent);
        e.data_start = min_opt(e.data_start, r.min_bucket);
        if let Some(b) = r.max_bucket {
            last_buckets.insert(r.parameter_id, b);
        }
        e.continuous_count = r.count;
    }
    for r in spot {
        let e = extents.entry(r.parameter_id).or_insert_with(empty_extent);
        e.data_start = min_opt(e.data_start, r.min_time);
        e.data_end = max_opt(e.data_end, r.max_time);
        e.spot_count = r.count;
    }
    for r in recent {
        let e = extents.entry(r.parameter_id).or_insert_with(empty_extent);
        e.data_start = min_opt(e.data_start, r.min_time);
        e.data_end = max_opt(e.data_end, r.max_time);
        // The raw pass overlaps hours the aggregate already covers, so it only speaks for a slot
        // the summaries report empty: it decides `has_*` for data too new to be summarised, it
        // never adds to a summarised count.
        if e.continuous_count == 0 {
            e.continuous_count = r.continuous_count;
        }
        if e.spot_count == 0 {
            e.spot_count = r.spot_count;
        }
    }
    for (parameter_id, t) in flagged_heads {
        let e = extents.entry(parameter_id).or_insert_with(empty_extent);
        e.data_start = min_opt(e.data_start, Some(t));
    }
    for (parameter_id, t) in cursors {
        let e = extents.entry(parameter_id).or_insert_with(empty_extent);
        e.data_end = max_opt(e.data_end, Some(t));
    }
    // An exact source (samples, the recent pass, a stream cursor) that has seen the newest
    // bucket names the true end; only a series none of them cover falls back to the bucket
    // plus its width, overstating by at most an hour rather than clipping the last hour off.
    for (parameter_id, bucket) in last_buckets {
        let e = extents.entry(parameter_id).or_insert_with(empty_extent);
        e.data_end = match e.data_end {
            Some(exact) if exact >= bucket => Some(exact),
            _ => max_opt(e.data_end, Some(bucket + chrono::Duration::hours(1))),
        };
    }
    for e in extents.values_mut() {
        e.reading_count = e.continuous_count + e.spot_count;
    }

    Ok(extents)
}

pub(super) fn empty_extent() -> ParameterExtent {
    ParameterExtent {
        data_start: None,
        data_end: None,
        reading_count: 0,
        spot_count: 0,
        continuous_count: 0,
    }
}

/// Declared cadence per site_parameter, for slots with no data yet: paired stream
/// declarations first, then the open deployment's sensor data_frequency.
pub(super) async fn declared_frequencies(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
) -> AppResult<HashMap<Uuid, &'static str>> {
    let mut map: HashMap<Uuid, &'static str> = HashMap::new();

    for row in DeployedFrequencyRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT d.parameter_id, bool_or(sn.data_frequency = 'low') AS any_low, \
                    bool_or(sn.data_frequency = 'high') AS any_high \
             FROM sensor_deployments d JOIN sensors sn ON sn.id = d.sensor_id \
             WHERE d.site_id = $1 AND d.deployed_until IS NULL \
             GROUP BY d.parameter_id",
        [site_id.into()],
    ))
    .all(db)
    .await?
    {
        if let Some(pid) = row.parameter_id {
            map.insert(
                pid,
                match (row.any_high, row.any_low) {
                    (false, true) => "low",
                    (true, true) => "mixed",
                    _ => "high",
                },
            );
        }
    }

    for row in DeclaredFrequencyRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT sp.parameter_id, bool_or(ds.measurement_type = 'spot') AS any_spot, \
                    bool_or(ds.measurement_type <> 'spot') AS any_continuous \
             FROM site_parameters sp JOIN data_streams ds ON ds.site_parameter_id = sp.id \
             WHERE sp.site_id = $1 AND ds.measurement_type IS NOT NULL \
             GROUP BY sp.parameter_id",
        [site_id.into()],
    ))
    .all(db)
    .await?
    {
        map.insert(
            row.parameter_id,
            match (row.any_continuous, row.any_spot) {
                (false, true) => "low",
                (true, true) => "mixed",
                _ => "high",
            },
        );
    }

    Ok(map)
}

/// Build a `ParameterResponse` from a site_parameter, enriched with the global catalog
/// (code/name/units) and the per-parameter reading extent.
pub(super) fn build_parameter_response(
    p: site_parameters::Model,
    globals: &HashMap<Uuid, site_parameters::CatalogParameter>,
    extents: &HashMap<Uuid, ParameterExtent>,
    declared: &HashMap<Uuid, &'static str>,
) -> ParameterResponse {
    let d = site_parameters::SlotDescriptor::resolve(&p, globals.get(&p.parameter_id));
    let extent = extents.get(&p.parameter_id);
    let has_spot = extent.is_some_and(|e| e.spot_count > 0);
    let has_continuous = extent.is_some_and(|e| e.continuous_count > 0);
    // Observed cadence when there is data; the DECLARED tier (stream declaration, sensor
    // data_frequency) for empty slots, so a new lab parameter opens on the right chart mode.
    let frequency = match (has_continuous, has_spot) {
        (false, true) => "low",
        (true, true) => "mixed",
        (true, false) => "high",
        (false, false) => declared.get(&p.parameter_id).copied().unwrap_or("high"),
    }
    .to_string();
    ParameterResponse {
        id: p.id,
        parameter_id: p.parameter_id,
        code: d.code,
        name: d.name,
        units: d.units,
        entry_mode: p.entry_mode.clone(),
        sensor_type: d.sensor_type,
        display_units: d.display_units,
        decimal_places: d.decimal_places,
        sample_interval_sec: p.sample_interval_sec,
        is_active: p.is_active,
        data_start: extent.and_then(|e| e.data_start),
        data_end: extent.and_then(|e| e.data_end),
        reading_count: extent.map(|e| e.reading_count),
        has_continuous,
        has_spot,
        frequency,
    }
}

// --- Readings ---

/// Where a row lands on the response's row axis.
///
/// Replicate groups share a timestamp, so the replicate view is keyed by `(time, replicate_index)`
/// and the collapsed view by time alone. One derivation either way, so the axis and the columns
/// cannot disagree. It was positional, which dated every parameter's values to whichever parameter
/// happened to have the most rows.
pub(super) enum RowIndex<'a> {
    ByReplicate(&'a HashMap<(DateTime<Utc>, i16), usize>),
    ByTime(&'a HashMap<DateTime<Utc>, usize>),
}

impl RowIndex<'_> {
    pub(super) fn of(&self, time: DateTime<Utc>, replicate_index: Option<i16>) -> Option<usize> {
        match self {
            Self::ByReplicate(index) => index.get(&(time, replicate_index.unwrap_or(0))).copied(),
            Self::ByTime(index) => index.get(&time).copied(),
        }
    }
}

/// Which optional annotations the caller asked for.
#[derive(Debug, Clone, Copy)]
pub(super) struct Annotations {
    pub(super) alarms: bool,
    pub(super) flagged: bool,
    pub(super) measurement_type: bool,
    pub(super) sample_stats: bool,
    pub(super) curves: bool,
    pub(super) origin: bool,
    /// Serve spot instants the source has retracted, marked as retracted, instead of omitting them.
    pub(super) withdrawn: bool,
}

/// The export projection: one column set built from the same `ParameterData` the JSON body
/// serialises, so an opt-in cannot be honoured in one format and dropped in another.
///
/// The value and parameter-id columns are the whole default header, grouped by kind across
/// parameters as they always were; every other kind appears only for the opt-in that asked for it.
pub(super) fn uuid_cells(ids: Option<&Vec<Option<Uuid>>>) -> Vec<Option<String>> {
    ids.map(|v| v.iter().map(|id| id.map(|id| id.to_string())).collect())
        .unwrap_or_default()
}

pub(super) fn readings_table(
    times: &[DateTime<Utc>],
    params: &[ParameterData],
    include_flags: bool,
    replicate_indices: Option<&[i16]>,
) -> Table {
    let mut table = Table::at(times);
    // A replicate export's rows are `(time, replicate_index)`; without the index column two rows
    // at one instant are indistinguishable in the file.
    if let Some(indices) = replicate_indices {
        table.column(
            "replicate_index".to_string(),
            Cells::Int(indices.iter().map(|i| Some(i64::from(*i))).collect()),
        );
    }
    for p in params {
        table.column(p.code.clone(), Cells::Float(p.values.clone()));
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
            table.column(
                format!("{}_source_system", p.code),
                Cells::Constant(sources),
            );
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
            // The join key to the replicates download, which is a row per replicate where this is
            // a row per instant.
            table.column(
                format!("{}_sample_id", p.code),
                Cells::Text(
                    stats
                        .iter()
                        .map(|s| s.as_ref().map(|s| s.sample_id.to_string()))
                        .collect(),
                ),
            );
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
            // The divisor beside the number it produced: an sd whose formula is not named is one
            // two readers can compare and disagree about, which is the whole of I6.
            table.column(
                format!("{}_sd_estimator", p.code),
                Cells::Text(
                    stats
                        .iter()
                        .map(|s| s.as_ref().map(|s| s.sd_estimator.clone()))
                        .collect(),
                ),
            );
            table.column(
                format!("{}_median", p.code),
                Cells::Float(
                    stats
                        .iter()
                        .map(|s| s.as_ref().and_then(|s| s.median))
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

/// Resolve sample rows and their replicate readings for the given sample ids in two batched
/// queries, keyed by sample id.
pub(super) async fn fetch_sample_stats(
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
                    stdev_sample: s.stdev_sample,
                    stdev_population: s.stdev_population,
                    median: s.median,
                    min: s.min_value,
                    max: s.max_value,
                    sd_estimator: s.sd_estimator,
                    sd_estimator_source: s.sd_estimator_source,
                    replicates: Vec::new(),
                },
            )
        })
        .collect();

    // The time bounds keep chunk exclusion in play; sample_id alone plans across every chunk.
    let mut replicates = readings::Entity::find()
        .select_only()
        .column(readings::Column::SampleId)
        .column(readings::Column::ReplicateIndex)
        .column(readings::Column::RawValue)
        .column(readings::Column::CalibratedValue)
        .column(readings::Column::IsFlagged)
        .column(readings::Column::CalibrationId)
        .column(readings::Column::StandardCurveId)
        .expr_as(
            Expr::col(readings::Column::WithdrawnAt).is_not_null(),
            "withdrawn",
        )
        .filter(readings::Column::SampleId.is_in(sample_ids.to_vec()))
        .filter(readings::Column::Time.gte(start))
        .order_by_asc(readings::Column::SampleId)
        .order_by_asc(readings::Column::ReplicateIndex);
    if let Some(end) = end {
        replicates = replicates.filter(readings::Column::Time.lte(end));
    }
    for row in replicates
        .into_model::<SampleReplicateRow>()
        .all(db)
        .await?
    {
        if let Some(stat) = stats.get_mut(&row.sample_id) {
            stat.replicates.push(ReplicateOut {
                replicate_index: row.replicate_index,
                raw_value: row.raw_value,
                calibrated_value: row.calibrated_value,
                calibration_id: row.calibration_id,
                standard_curve_id: row.standard_curve_id,
                flagged: row.is_flagged.unwrap_or(false),
                withdrawn: row.withdrawn,
            });
        }
    }

    Ok(stats)
}

pub(super) async fn count_withdrawn_instants(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    parameter_ids: &[Uuid],
    start: DateTime<Utc>,
    end: Option<DateTime<Utc>>,
) -> Result<HashMap<Uuid, i64>, AppError> {
    if parameter_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let (sql, values) =
        withdrawn_instants_query(site_id, parameter_ids, start, end).build(PostgresQueryBuilder);
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values.0,
        ))
        .await?;
    let mut counts = HashMap::with_capacity(rows.len());
    for row in &rows {
        let withdrawn = WithdrawnCount::from_query_result(row, "")?;
        counts.insert(withdrawn.parameter_id, withdrawn.withdrawn_count);
    }
    Ok(counts)
}

/// Fully withdrawn spot instants per parameter, counted once each.
///
/// An instant is withdrawn only when every replicate in it is, so the grouping happens in the
/// subquery and the outer count is over instants rather than rows. The time bounds are on the
/// inner query, where chunk exclusion can see them.
pub(super) fn withdrawn_instants_query(
    site_id: Uuid,
    parameter_ids: &[Uuid],
    start: DateTime<Utc>,
    end: Option<DateTime<Utc>>,
) -> SelectStatement {
    let mut instants = SeaQuery::select();
    instants
        .column(readings::Column::ParameterId)
        .column(readings::Column::Time)
        .from(readings::Entity)
        .and_where(Expr::col(readings::Column::SiteId).eq(site_id))
        .and_where(Expr::col(readings::Column::ParameterId).is_in(parameter_ids.to_vec()))
        .and_where(Expr::col(readings::Column::MeasurementType).eq("spot"))
        .and_where(Expr::col(readings::Column::Time).gte(start))
        .add_group_by([
            Expr::col(readings::Column::ParameterId),
            Expr::col(readings::Column::Time),
        ])
        .and_having(Expr::cust("bool_and(withdrawn_at IS NOT NULL)"));
    if let Some(end) = end {
        instants.and_where(Expr::col(readings::Column::Time).lte(end));
    }

    let mut counts = SeaQuery::select();
    counts
        .column(readings::Column::ParameterId)
        .expr_as(Expr::cust("count(*)"), Alias::new("withdrawn_count"))
        .from_subquery(instants, Alias::new("g"))
        .add_group_by([Expr::col(readings::Column::ParameterId)]);
    counts
}

// --- Aggregates ---

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

/// The export projection, built from the same structs the JSON body serialises.
pub(super) fn aggregates_table(
    times: &[DateTime<Utc>],
    params: &[ParameterAggregateData],
) -> Table {
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

// --- Status events ---

/// One line-oriented body from an iterator of rendered lines, each already newline-terminated.
pub(super) fn line_stream(content_type: &'static str, lines: Vec<String>) -> AppResult<Response> {
    let lines = lines.into_iter().map(Ok::<_, std::io::Error>);
    Response::builder()
        .header(header::CONTENT_TYPE, HeaderValue::from_static(content_type))
        .body(axum::body::Body::from_stream(futures::stream::iter(lines)))
        .map_err(|e| AppError::Internal(e.to_string()))
}

/// Build a streaming CSV response for status events.
pub(super) fn build_status_events_csv(events: &[StatusEventData]) -> AppResult<Response> {
    let mut lines = vec!["time,parameter_id,value,sensor_id\n".to_string()];
    lines.extend(events.iter().map(|event| {
        crate::common::csv::row_to_string([
            event.time.to_rfc3339(),
            event.parameter_id.to_string(),
            event.value.clone(),
            event.sensor_id.map(|id| id.to_string()).unwrap_or_default(),
        ])
    }));
    line_stream("text/csv", lines)
}

/// Build a streaming NDJSON response for status events.
pub(super) fn build_status_events_ndjson(events: &[StatusEventData]) -> AppResult<Response> {
    let lines = events
        .iter()
        .map(|event| format!("{}\n", serde_json::to_string(event).unwrap_or_default()))
        .collect();
    line_stream("application/x-ndjson", lines)
}

// --- Statistics ---

/// The value expression each cadence is summarised over.
///
/// Continuous rows live at `replicate_index = 0` and are the reading itself. A spot instant is
/// summarised at the value the API serves for it, the sample mean over the live replicates, so the
/// period statistics and the plotted series are the same numbers.
pub(super) fn value_source(
    measurement_type: &str,
    site_id: Uuid,
    parameter_ids: &[Uuid],
) -> SelectStatement {
    let r = Alias::new("r");
    let mut source = SeaQuery::select();
    source
        .column((r.clone(), readings::Column::ParameterId))
        .column((r.clone(), readings::Column::Time))
        .from_as(readings::Entity, r.clone())
        .and_where(Expr::col((r.clone(), readings::Column::SiteId)).eq(site_id))
        .and_where(
            Expr::col((r.clone(), readings::Column::ParameterId)).is_in(parameter_ids.to_vec()),
        )
        .and_where(
            Expr::col((r.clone(), readings::Column::IsFlagged))
                .ne(true)
                .or(Expr::col((r.clone(), readings::Column::IsFlagged)).is_null()),
        )
        .add_group_by([
            Expr::col((r.clone(), readings::Column::ParameterId)).into(),
            Expr::col((r.clone(), readings::Column::Time)).into(),
        ]);
    if measurement_type == "spot" {
        let smp = Alias::new("smp");
        source
            .expr_as(
                Expr::cust(
                    "COALESCE(MAX(smp.mean), AVG(COALESCE(r.calibrated_value, r.raw_value)))",
                ),
                Alias::new("value"),
            )
            .join_as(
                JoinType::LeftJoin,
                samples::Entity,
                smp.clone(),
                Expr::col((smp, samples::Column::Id))
                    .equals((r.clone(), readings::Column::SampleId)),
            )
            .and_where(Expr::col((r.clone(), readings::Column::MeasurementType)).eq("spot"))
            .and_where(Expr::col((r, readings::Column::WithdrawnAt)).is_null());
    } else {
        // Several streams can serve one slot instant; the period counts the instant once.
        source
            .expr_as(
                Expr::cust("AVG(COALESCE(r.calibrated_value, r.raw_value))"),
                Alias::new("value"),
            )
            .and_where(Expr::col((r.clone(), readings::Column::ReplicateIndex)).eq(0))
            .and_where(Expr::cust("r.measurement_type IS DISTINCT FROM 'spot'"));
    }
    source
}

// --- Sensor identity bands ---

pub(super) fn parse_uuid_csv(s: &str) -> Vec<Uuid> {
    s.split(',')
        .filter_map(|p| Uuid::parse_str(p.trim()).ok())
        .collect()
}

#[cfg(test)]
#[path = "tests/service.rs"]
mod tests;
