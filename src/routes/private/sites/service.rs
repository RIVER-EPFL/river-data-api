//! The queries and resolution logic behind the site-scoped endpoints.

use std::collections::HashMap;

use axum::http::header::{self, HeaderValue};
use axum::response::Response;
use chrono::{DateTime, Utc};
use crudcrate::{ApiError, CRUDOperations};
use sea_orm::sea_query::{
    Alias, Expr, ExprTrait, Func, JoinType, PostgresQueryBuilder, Query as SeaQuery,
    SelectStatement, UnionType,
};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, EntityTrait, FromQueryResult, Order, QueryFilter,
    QueryOrder, QuerySelect, Statement, TransactionTrait,
};
use uuid::Uuid;

use super::models::*;
use crate::common::aggregates::Resolution;
use crate::common::series::{Cells, Table};
use crate::common::served;
use crate::error::{AppError, AppResult};
use crate::routes::private::meteoswiss::models::ExternalSource;
use crate::routes::private::readings::models as readings;
use crate::routes::private::readings::samples;
use crate::routes::private::sensor_calibrations::models as sensor_calibrations;
use crate::routes::private::sensor_deployments::models as sensor_deployments;
use crate::routes::private::sensors::models as sensors;
use crate::routes::private::site_parameters;

// --- The sites entity's hooks ---

/// A site's columns are what a formula reads as a site property, and the change-audit trigger
/// records every edit to them under the writer the transaction names.
pub struct SiteOperations;

impl CRUDOperations for SiteOperations {
    type Resource = Site;

    /// The change-audit trigger reads the writer from the transaction, so the label is declared on
    /// every write this entity makes, before any hook or statement on it.
    async fn after_begin<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
    ) -> Result<(), ApiError> {
        crate::common::actor::declare(db)
            .await
            .map_err(ApiError::database)
    }
}

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

/// Build a `ParameterResponse` from a site_parameter, enriched with the global catalog
/// (code/name/units) and the per-parameter reading extent.
pub(super) fn build_parameter_response(
    p: site_parameters::Model,
    globals: &HashMap<Uuid, site_parameters::CatalogParameter>,
    extents: &HashMap<Uuid, ParameterExtent>,
    attributions: &HashMap<Uuid, ExternalSource>,
) -> ParameterResponse {
    let d = site_parameters::SlotDescriptor::resolve(&p, globals.get(&p.parameter_id));
    let extent = extents.get(&p.parameter_id);
    let has_spot = extent.is_some_and(|e| e.spot_count > 0);
    let has_continuous = extent.is_some_and(|e| e.continuous_count > 0);
    // The slot's own declaration, not what its rows happen to hold: a slot flipped to grab
    // sampling keeps the logger week it already carries, and the declaration is what the chain
    // and the stream engine divide on.
    let frequency = p.cadence.clone();
    ParameterResponse {
        id: p.id,
        parameter_id: p.parameter_id,
        code: d.code,
        name: d.name,
        units: d.units,
        entry_mode: p.entry_mode.clone(),
        sensor_type: d.sensor_type,
        decimal_places: d.decimal_places,
        sample_interval_sec: p.sample_interval_sec,
        is_active: p.is_active,
        data_start: extent.and_then(|e| e.data_start),
        data_end: extent.and_then(|e| e.data_end),
        reading_count: extent.map(|e| e.reading_count),
        has_continuous,
        has_spot,
        frequency,
        external_source: attributions.get(&p.parameter_id).cloned(),
    }
}

// --- Readings ---

/// What a site readings request asks for, decided from its query before anything is read.
#[derive(Debug, Clone)]
pub(super) struct ReadingsRequest {
    pub(super) start: DateTime<Utc>,
    pub(super) end: Option<DateTime<Utc>>,
    /// A row per replicate instead of a row per instant; naming a `sample_id` asks for it too.
    pub(super) replicates: bool,
    pub(super) annotations: Annotations,
    /// The export's per-point flag columns.
    pub(super) flag_columns: bool,
    /// The one measurement type served, empty for every type.
    pub(super) measurement_type: String,
    pub(super) sample_id: Option<Uuid>,
}

impl ReadingsRequest {
    /// The request a query makes. A window naming no start opens `lookback_days` before `now`.
    pub(super) fn from_query(
        query: &SiteReadingsQuery,
        lookback_days: i64,
        now: DateTime<Utc>,
    ) -> AppResult<Self> {
        let start = query
            .start
            .unwrap_or_else(|| now - chrono::Duration::days(lookback_days));
        crate::routes::validate_optional_time_range(Some(start), query.end)?;
        let replicates = query.include_replicates.unwrap_or(false) || query.sample_id.is_some();
        // Replicates and statistics never share one file (Q32): one is a row per replicate, the
        // other a row per instant, and a file carrying both is neither.
        if replicates && query.include_sample_stats.unwrap_or(false) {
            return Err(AppError::BadRequest(
                "include_sample_stats and include_replicates cannot be combined: the replicate \
                 rows and the per-instant statistics are separate files. Request the statistics \
                 here, and the replicates as their own download."
                    .to_string(),
            ));
        }
        let measurement_type = query.measurement_type.clone().unwrap_or_default();
        if !measurement_type.is_empty() {
            crate::routes::private::readings::service::validate_measurement_type(Some(
                &measurement_type,
            ))?;
        }
        Ok(Self {
            start,
            end: query.end,
            replicates,
            annotations: Annotations {
                alarms: query.alarms.unwrap_or(false),
                flagged: query.include_flagged.unwrap_or(true),
                measurement_type: query.include_measurement_type.unwrap_or(false),
                sample_stats: query.include_sample_stats.unwrap_or(false),
                curves: query.include_curves.unwrap_or(false),
                origin: query.include_origin.unwrap_or(false),
                withdrawn: query.include_withdrawn.unwrap_or(false),
            },
            flag_columns: query.include_flags.unwrap_or(false),
            measurement_type,
            sample_id: query.sample_id,
        })
    }

    /// Whether the window's fully withdrawn spot instants are counted: a retracted visit is a fact
    /// about the window whether or not its points are drawn, so on every collapsed view but the
    /// continuous one.
    pub(super) fn counts_withdrawn(&self) -> bool {
        !self.replicates && self.measurement_type != "continuous"
    }
}

/// The sensor types a comma-separated `sensor_types` names.
pub(super) fn sensor_type_filter(types: Option<&str>) -> Option<Vec<String>> {
    types.map(|list| list.split(',').map(|s| s.trim().to_string()).collect())
}

/// The parameter ids a comma-separated `parameter_ids` names. A list naming none is refused rather
/// than read as no filter.
pub(super) fn parameter_id_filter(ids: Option<&str>) -> AppResult<Option<Vec<Uuid>>> {
    let Some(list) = ids else {
        return Ok(None);
    };
    let parsed: Vec<Uuid> = list
        .split(',')
        .filter_map(|s| Uuid::parse_str(s.trim()).ok())
        .collect();
    if parsed.is_empty() {
        return Err(AppError::BadRequest(
            "parameter_ids was provided but no UUIDs could be parsed".to_string(),
        ));
    }
    Ok(Some(parsed))
}

/// The arms of the collapsed series a measurement-type filter reaches.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct SeriesArms {
    pub(super) continuous: bool,
    pub(super) spot: bool,
    /// A continuous-shaped type the continuous arm is narrowed to.
    pub(super) continuous_type: Option<String>,
}

/// "continuous" folds in derived and legacy NULL rows, matching the continuous aggregates, which
/// exclude only 'spot'; any other named type is continuous-shaped and narrows the continuous arm.
pub(super) fn series_arms(measurement_type: &str) -> SeriesArms {
    let (continuous, spot, continuous_type) = match measurement_type {
        "" => (true, true, None),
        "continuous" => (true, false, None),
        "spot" => (false, true, None),
        other => (true, false, Some(other.to_string())),
    };
    SeriesArms {
        continuous,
        spot,
        continuous_type,
    }
}

/// The readings a request reaches at the site: its parameters, its window, and the flag and
/// sample filters it asked for.
fn requested_rows(site_id: Uuid, parameter_ids: &[Uuid], request: &ReadingsRequest) -> Condition {
    let r = served::r();
    let mut rows = Condition::all()
        .add(Expr::col((r.clone(), readings::Column::SiteId)).eq(site_id))
        .add(Expr::col((r.clone(), readings::Column::ParameterId)).is_in(parameter_ids.to_vec()))
        .add(Expr::col((r.clone(), readings::Column::Time)).gte(request.start));
    if let Some(end) = request.end {
        rows = rows.add(Expr::col((r.clone(), readings::Column::Time)).lte(end));
    }
    if !request.annotations.flagged {
        rows = rows.add(served::not_flagged());
    }
    if let Some(sample_id) = request.sample_id {
        rows = rows.add(Expr::col((r, readings::Column::SampleId)).eq(sample_id));
    }
    rows
}

/// A row's alarm severity from the one shared ladder over `value`, NULL when alarms were not
/// asked for or the slot has no threshold at any tier (no `t` row). The ladder treats all-NULL
/// bounds as 0 (disabled).
fn severity_column(value: &str, alarms: bool) -> Expr {
    if !alarms {
        return Expr::cust("NULL::smallint");
    }
    let severity = crate::routes::private::alarms::service::severity_case(
        value,
        "t.warning_min",
        "t.warning_max",
        "t.alarm_min",
        "t.alarm_max",
    );
    Expr::cust(format!(
        "CASE WHEN t.parameter_id IS NULL THEN NULL ELSE ({severity})::smallint END"
    ))
}

/// LEFT JOINs each slot's threshold (site, then global, then the parameter default), scoped to the
/// site, as `t` onto the parameter and site of `on`, so a parameter with only defaults still gets a
/// severity.
fn join_thresholds(q: &mut SelectStatement, site_id: Uuid, parameter_ids: &[Uuid], on: &Alias) {
    let t = Alias::new("t");
    let column = |alias: &Alias, name: &str| Expr::col((alias.clone(), Alias::new(name)));
    q.join_subquery(
        JoinType::LeftJoin,
        crate::routes::private::alarms::service::resolve_thresholds_query(
            Some(site_id),
            Some(parameter_ids.to_vec()),
        ),
        t.clone(),
        Condition::all()
            .add(column(&t, "parameter_id").equals((on.clone(), Alias::new("parameter_id"))))
            .add(column(&t, "site_id").equals((on.clone(), Alias::new("site_id")))),
    );
}

/// Every stored row the request reaches, one per replicate, in axis order; the caller
/// reconstructs the groups. "continuous" means everything that is not a grab, as on the collapsed
/// series. A retracted row is exported only when asked for, as every other serving path does.
pub(super) fn replicate_rows_query(
    site_id: Uuid,
    parameter_ids: &[Uuid],
    request: &ReadingsRequest,
) -> SelectStatement {
    let r = served::r();
    let mut rows = requested_rows(site_id, parameter_ids, request);
    match request.measurement_type.as_str() {
        "" => {}
        "continuous" => {
            rows = rows.add(Expr::cust("(r.measurement_type IS DISTINCT FROM 'spot')"));
        }
        other => {
            rows = rows.add(
                Expr::col((r.clone(), readings::Column::MeasurementType)).eq(other.to_string()),
            );
        }
    }
    if !request.annotations.withdrawn {
        rows = rows.add(Expr::col((r.clone(), readings::Column::WithdrawnAt)).is_null());
    }
    let mut replicates = SeaQuery::select();
    replicates
        .column((r.clone(), readings::Column::ParameterId))
        .column((r.clone(), readings::Column::Time))
        .column((r.clone(), readings::Column::ReplicateIndex))
        .column((r.clone(), readings::Column::StreamId))
        .expr_as(served::continuous_value(), Alias::new("value"))
        .expr_as(
            severity_column(served::CONTINUOUS_VALUE, request.annotations.alarms),
            Alias::new("severity"),
        )
        .column((r.clone(), readings::Column::IsFlagged))
        .column((r.clone(), readings::Column::FlagReason))
        .column((r.clone(), readings::Column::MeasurementType))
        .column((r.clone(), readings::Column::Unverified))
        .column((r.clone(), readings::Column::SampleId))
        .column((r.clone(), readings::Column::CalibrationId))
        .column((r.clone(), readings::Column::StandardCurveId))
        .expr_as(
            Expr::col((r.clone(), readings::Column::WithdrawnAt)).is_not_null(),
            Alias::new("withdrawn"),
        )
        .from_as(readings::Entity, r.clone());
    if request.annotations.alarms {
        join_thresholds(&mut replicates, site_id, parameter_ids, &r);
    }
    replicates
        .cond_where(rows)
        .order_by((r.clone(), readings::Column::ParameterId), Order::Asc)
        .order_by((r.clone(), readings::Column::Time), Order::Asc)
        .order_by((r, readings::Column::ReplicateIndex), Order::Asc);
    replicates
}

/// The columns both arms of the collapsed series select, in the order the union lines them up.
const SERVED_COLUMNS: [&str; 13] = [
    "value",
    "parameter_id",
    "time",
    "stream_id",
    "site_id",
    "is_flagged",
    "flag_reason",
    "measurement_type",
    "sample_id",
    "calibration_id",
    "standard_curve_id",
    "withdrawn",
    "unverified",
];

/// Every column of [`SERVED_COLUMNS`] after `value`, read off the readings row.
fn select_served_row(q: &mut SelectStatement) {
    let r = served::r();
    q.column((r.clone(), readings::Column::ParameterId))
        .column((r.clone(), readings::Column::Time))
        .column((r.clone(), readings::Column::StreamId))
        .column((r.clone(), readings::Column::SiteId))
        .column((r.clone(), readings::Column::IsFlagged))
        .column((r.clone(), readings::Column::FlagReason))
        .column((r.clone(), readings::Column::MeasurementType))
        .column((r.clone(), readings::Column::SampleId))
        .column((r.clone(), readings::Column::CalibrationId))
        .column((r.clone(), readings::Column::StandardCurveId))
        .expr_as(
            Expr::col((r.clone(), readings::Column::WithdrawnAt)).is_not_null(),
            Alias::new("withdrawn"),
        )
        .column((r, readings::Column::Unverified));
}

/// Continuous and derived rows at replicate_index 0, where every continuous writer puts them, so
/// the plain equality keeps the ordered scan. An instant two streams feed is collapsed after the
/// fetch.
fn continuous_arm(
    site_id: Uuid,
    parameter_ids: &[Uuid],
    request: &ReadingsRequest,
    narrowed_to: Option<&str>,
) -> SelectStatement {
    let r = served::r();
    let mut rows = requested_rows(site_id, parameter_ids, request).add(served::continuous_rows());
    if let Some(kind) = narrowed_to {
        rows = rows.add(Expr::col((r.clone(), readings::Column::MeasurementType)).eq(kind));
    }
    let mut arm = SeaQuery::select();
    arm.expr_as(served::continuous_value(), Alias::new("value"));
    select_served_row(&mut arm);
    arm.from_as(readings::Entity, r).cond_where(rows);
    arm
}

/// One row per spot instant, not per stream: the replicate group at the slot instant, served at
/// the sample mean over its unflagged replicates with the lowest unflagged replicate's own value as
/// the no-sample fallback. The key and its ordering are `common::served`, shared with the public
/// arm and the alarm evaluator. A retracted instant is served only when asked for, and the ordering
/// then prefers a live replicate, so `withdrawn` on the served row means the whole group is.
fn spot_arm(site_id: Uuid, parameter_ids: &[Uuid], request: &ReadingsRequest) -> SelectStatement {
    let r = served::r();
    let mut rows = requested_rows(site_id, parameter_ids, request)
        .add(Expr::col((r.clone(), readings::Column::MeasurementType)).eq("spot"));
    if !request.annotations.withdrawn {
        rows = rows.add(Expr::col((r.clone(), readings::Column::WithdrawnAt)).is_null());
    }
    let smp = Alias::new("smp");
    let mut group = SeaQuery::select();
    group
        .distinct_on(served::spot_instant_key())
        .expr_as(served::spot_value(), Alias::new("value"));
    select_served_row(&mut group);
    group
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::LeftJoin,
            samples::Entity,
            smp.clone(),
            Expr::col((smp, samples::Column::Id)).equals((r, readings::Column::SampleId)),
        )
        .cond_where(rows);
    for (expr, order) in served::spot_instant_order() {
        group.order_by_expr(expr, order);
    }
    let sp = Alias::new("sp");
    SeaQuery::select()
        .columns(SERVED_COLUMNS.map(|c| (sp.clone(), Alias::new(c))))
        .from_subquery(group, sp)
        .take()
}

/// The collapsed series: one row per continuous row and per spot instant over the arms the
/// request's filter reaches, ordered by parameter and time.
pub(super) fn served_series_query(
    site_id: Uuid,
    parameter_ids: &[Uuid],
    request: &ReadingsRequest,
) -> SelectStatement {
    let reach = series_arms(&request.measurement_type);
    let mut arms: Vec<SelectStatement> = Vec::new();
    if reach.continuous {
        arms.push(continuous_arm(
            site_id,
            parameter_ids,
            request,
            reach.continuous_type.as_deref(),
        ));
    }
    if reach.spot {
        arms.push(spot_arm(site_id, parameter_ids, request));
    }
    let mut arms = arms.into_iter();
    let first = arms.next().unwrap_or_default();
    let union = arms.fold(first, |mut acc, arm| acc.union(UnionType::All, arm).take());

    let sv = Alias::new("sv");
    let column = |name: &str| (sv.clone(), Alias::new(name));
    let mut series = SeaQuery::select();
    series
        .column(column("parameter_id"))
        .column(column("time"))
        .expr_as(Expr::cust("NULL::smallint"), Alias::new("replicate_index"))
        .column(column("stream_id"))
        .column(column("value"))
        .expr_as(
            severity_column("sv.value", request.annotations.alarms),
            Alias::new("severity"),
        )
        .columns(
            [
                "is_flagged",
                "flag_reason",
                "measurement_type",
                "sample_id",
                "calibration_id",
                "standard_curve_id",
                "withdrawn",
                "unverified",
            ]
            .map(column),
        )
        .from_subquery(union, sv.clone());
    if request.annotations.alarms {
        join_thresholds(&mut series, site_id, parameter_ids, &sv);
    }
    series
        .order_by(column("parameter_id"), Order::Asc)
        .order_by(column("time"), Order::Asc);
    series
}

/// The response's row axis: every served row's key, sorted.
///
/// Replicate groups share a timestamp, so the replicate view is keyed by `(time, replicate_index)`
/// and the collapsed view by time alone. The axis is the union of every parameter's keys, so one
/// parameter's values are never dated to another's instants.
pub(super) struct RowAxis {
    replicates: bool,
    keys: Vec<(DateTime<Utc>, i16)>,
    positions: HashMap<(DateTime<Utc>, i16), usize>,
}

impl RowAxis {
    pub(super) fn over(rows: &[ReadingRow], replicates: bool) -> Self {
        let mut keys: Vec<(DateTime<Utc>, i16)> = rows
            .iter()
            .map(|row| Self::key(row, replicates))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        keys.sort_unstable();
        let positions = keys.iter().enumerate().map(|(i, k)| (*k, i)).collect();
        Self {
            replicates,
            keys,
            positions,
        }
    }

    fn key(row: &ReadingRow, replicates: bool) -> (DateTime<Utc>, i16) {
        let index = if replicates {
            row.replicate_index.unwrap_or(0)
        } else {
            0
        };
        (row.time.with_timezone(&Utc), index)
    }

    /// Where a row lands on the axis.
    pub(super) fn position(&self, row: &ReadingRow) -> Option<usize> {
        self.positions
            .get(&Self::key(row, self.replicates))
            .copied()
    }

    pub(super) fn len(&self) -> usize {
        self.keys.len()
    }

    pub(super) fn times(&self) -> Vec<DateTime<Utc>> {
        self.keys.iter().map(|(t, _)| *t).collect()
    }

    /// Each row's replicate index, on the replicate view only.
    pub(super) fn replicate_indices(&self) -> Option<Vec<i16>> {
        self.replicates
            .then(|| self.keys.iter().map(|(_, i)| *i).collect())
    }
}

/// What a slot's series carries beyond its own rows, each present only when it was asked for.
pub(super) struct SeriesContext {
    pub(super) sample_stats: HashMap<Uuid, SampleStatOut>,
    /// The streams paired into each slot, keyed by site-parameter id.
    pub(super) origins: Option<HashMap<Uuid, Vec<OriginRef>>>,
    /// Fully withdrawn spot instants, keyed by catalog parameter id.
    pub(super) withdrawn_counts: Option<HashMap<Uuid, i64>>,
}

/// One slot's per-point columns over the row axis.
#[derive(Default)]
struct SeriesColumns {
    values: Vec<Option<f64>>,
    severities: Option<Vec<Option<i16>>>,
    flagged: Option<Vec<Option<bool>>>,
    flag_reasons: Option<Vec<Option<String>>>,
    measurement_types: Option<Vec<Option<String>>>,
    calibration_ids: Option<Vec<Option<Uuid>>>,
    standard_curve_ids: Option<Vec<Option<Uuid>>>,
    samples: Option<Vec<Option<SampleStatOut>>>,
    withdrawn: Option<Vec<Option<bool>>>,
    unverified: Vec<Option<bool>>,
}

impl SeriesColumns {
    fn fill(
        rows: &[ReadingRow],
        axis: &RowAxis,
        annotations: Annotations,
        sample_stats: &HashMap<Uuid, SampleStatOut>,
    ) -> Self {
        let len = axis.len();
        let mut columns = Self {
            values: vec![None; len],
            severities: annotations.alarms.then(|| vec![None; len]),
            flagged: annotations.flagged.then(|| vec![None; len]),
            flag_reasons: annotations.flagged.then(|| vec![None; len]),
            measurement_types: annotations.measurement_type.then(|| vec![None; len]),
            calibration_ids: annotations.curves.then(|| vec![None; len]),
            standard_curve_ids: annotations.curves.then(|| vec![None; len]),
            samples: annotations.sample_stats.then(|| vec![None; len]),
            withdrawn: annotations.withdrawn.then(|| vec![None; len]),
            unverified: vec![None; len],
        };
        for row in rows {
            let Some(i) = axis.position(row) else {
                continue;
            };
            columns.values[i] = Some(row.value);
            columns.unverified[i] = row.unverified;
            if let Some(v) = columns.severities.as_mut() {
                v[i] = row.severity;
            }
            if let Some(v) = columns.flagged.as_mut() {
                v[i] = row.is_flagged;
            }
            if let Some(v) = columns.flag_reasons.as_mut() {
                v[i].clone_from(&row.flag_reason);
            }
            if let Some(v) = columns.measurement_types.as_mut() {
                v[i].clone_from(&row.measurement_type);
            }
            if let Some(v) = columns.calibration_ids.as_mut() {
                v[i] = row.calibration_id;
            }
            if let Some(v) = columns.standard_curve_ids.as_mut() {
                v[i] = row.standard_curve_id;
            }
            if let Some(v) = columns.samples.as_mut() {
                v[i] = row.sample_id.and_then(|id| sample_stats.get(&id).cloned());
            }
            if let Some(v) = columns.withdrawn.as_mut() {
                v[i] = row.withdrawn;
            }
        }
        columns
    }
}

/// Each slot's series over the row axis, in slot order.
pub(super) fn slot_series(
    slots: &[site_parameters::Model],
    catalog: &HashMap<Uuid, site_parameters::CatalogParameter>,
    rows: Vec<ReadingRow>,
    axis: &RowAxis,
    annotations: Annotations,
    context: &SeriesContext,
) -> Vec<ParameterData> {
    let mut by_parameter: HashMap<Uuid, Vec<ReadingRow>> = HashMap::new();
    for row in rows {
        by_parameter.entry(row.parameter_id).or_default().push(row);
    }
    slots
        .iter()
        .map(|slot| {
            let rows = by_parameter
                .get(&slot.parameter_id)
                .map_or(&[][..], Vec::as_slice);
            let columns = SeriesColumns::fill(rows, axis, annotations, &context.sample_stats);
            let descriptor =
                site_parameters::SlotDescriptor::resolve(slot, catalog.get(&slot.parameter_id));
            ParameterData {
                id: slot.id,
                parameter_id: slot.parameter_id,
                code: descriptor.code,
                name: descriptor.slot_name,
                display_name: descriptor.catalog_name,
                sensor_type: descriptor.sensor_type,
                units: descriptor.units,
                decimal_places: descriptor.decimal_places,
                values: columns.values,
                severities: columns.severities,
                flagged: columns.flagged,
                flag_reasons: columns.flag_reasons,
                measurement_types: columns.measurement_types,
                calibration_ids: columns.calibration_ids,
                standard_curve_ids: columns.standard_curve_ids,
                samples: columns.samples,
                origins: context
                    .origins
                    .as_ref()
                    .map(|o| o.get(&slot.id).cloned().unwrap_or_default()),
                withdrawn: columns.withdrawn,
                unverified: Some(columns.unverified),
                withdrawn_count: context
                    .withdrawn_counts
                    .as_ref()
                    .map(|c| c.get(&slot.parameter_id).copied().unwrap_or(0)),
            }
        })
        .collect()
}

/// The readings body over a row axis, its range the axis's first and last instant.
pub(super) fn readings_response(
    project: Option<ProjectRef>,
    site: SiteRef,
    axis: &RowAxis,
    parameters: Vec<ParameterData>,
) -> ReadingsResponse {
    let times = axis.times();
    ReadingsResponse {
        project,
        site,
        start: times.first().copied(),
        end: times.last().copied(),
        times,
        replicate_indices: axis.replicate_indices(),
        parameters,
    }
}

/// The body of a request for a site with no slot the query selects: no rows, no range.
pub(super) fn empty_readings_response(
    project: Option<ProjectRef>,
    site: SiteRef,
) -> ReadingsResponse {
    ReadingsResponse {
        project,
        site,
        start: None,
        end: None,
        times: Vec::new(),
        replicate_indices: None,
        parameters: Vec::new(),
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

pub(super) fn uuid_cells(ids: Option<&Vec<Option<Uuid>>>) -> Vec<Option<String>> {
    ids.map(|v| v.iter().map(|id| id.map(|id| id.to_string())).collect())
        .unwrap_or_default()
}

/// The export projection: one column set built from the same `ParameterData` the JSON body
/// serialises, so an opt-in cannot be honoured in one format and dropped in another.
///
/// The value and pending-entry columns are the whole default header, grouped by kind across
/// parameters; every other kind appears only for the opt-in that asked for it.
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
    if params.iter().any(|p| p.withdrawn.is_some()) {
        for p in params {
            table.column(
                format!("{}_withdrawn", p.code),
                Cells::Bool(p.withdrawn.clone().unwrap_or_default()),
            );
        }
    }
    for p in params {
        table.column(
            format!("{}_unverified", p.code),
            Cells::Bool(p.unverified.clone().unwrap_or_default()),
        );
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
                    median: s.median,
                    min: s.min_value,
                    max: s.max_value,
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
        "6hourly" => Some(Resolution::SixHourly),
        "12hourly" => Some(Resolution::TwelveHourly),
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
        Resolution::SixHourly => "6 hours",
        Resolution::TwelveHourly => "12 hours",
        Resolution::Daily => "1 day",
        Resolution::Weekly => "7 days",
        Resolution::Monthly => "1 month",
    }
}

/// The rollup read behind the aggregates endpoint: `$1` site, `$2` parameter ids, `$3`/`$4` the
/// first and last bucket served.
///
/// The collapsed read sums the sensor dimension away (count-weighted avg, MIN/MAX, SUM(count)) and
/// selects a NULL `sensor_id`; `split` keeps it. One text either way, so the two reads cannot drift.
#[must_use]
pub(super) fn aggregate_buckets_sql(rollup: Resolution, split: bool) -> String {
    let (sensor_select, sensor_group) = if split {
        ("sensor_id", ", sensor_id")
    } else {
        ("NULL::uuid AS sensor_id", "")
    };
    format!(
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
          AND bucket >= $3
          AND bucket <= $4
        GROUP BY bucket, parameter_id{sensor_group}
        ORDER BY bucket ASC, parameter_id ASC{sensor_group}
        ",
        view = rollup.view(),
    )
}

/// Flagged continuous readings per served bucket, keyed like [`aggregate_buckets_sql`]'s rows.
///
/// Bounded by bucket, as the rollup read is, so the last bucket is counted to its end; the raw
/// `time` bounds one width past the window are what chunk exclusion sees.
#[must_use]
pub(super) fn flagged_buckets_query(
    rollup: Resolution,
    site_id: Uuid,
    parameter_ids: &[Uuid],
    split: bool,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> SelectStatement {
    let width = bucket_interval(rollup);
    let bucket = || {
        Expr::from(
            Func::cust("time_bucket")
                .arg(Expr::val(width).cast_as("interval"))
                .arg(Expr::col(readings::Column::Time)),
        )
    };
    let past_end = Expr::val(end)
        .cast_as("timestamptz")
        .add(Expr::val(width).cast_as("interval"));
    let mut flagged = SeaQuery::select();
    flagged.expr_as(bucket(), Alias::new("bucket"));
    flagged.column(readings::Column::ParameterId);
    if split {
        flagged.column(readings::Column::SensorId);
    } else {
        flagged.expr_as(Expr::cust("NULL::uuid"), Alias::new("sensor_id"));
    }
    flagged
        .expr_as(Expr::cust("COUNT(*)::bigint"), Alias::new("flagged_count"))
        .from(readings::Entity)
        .and_where(Expr::col(readings::Column::SiteId).eq(site_id))
        .and_where(Expr::col(readings::Column::ParameterId).is_in(parameter_ids.to_vec()))
        .and_where(Expr::col(readings::Column::Time).gte(start))
        .and_where(Expr::col(readings::Column::Time).lt(past_end))
        .and_where(bucket().gte(start))
        .and_where(bucket().lte(end))
        .and_where(Expr::col(readings::Column::IsFlagged).eq(true))
        .and_where(Expr::col(readings::Column::ReplicateIndex).eq(0))
        .and_where(Expr::cust("measurement_type IS DISTINCT FROM 'spot'"))
        .add_group_by([
            Expr::col(Alias::new("bucket")),
            Expr::col(readings::Column::ParameterId),
        ]);
    if split {
        flagged.add_group_by([Expr::col(readings::Column::SensorId)]);
    }
    flagged.take()
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
            Expr::col((r.clone(), readings::Column::ParameterId)),
            Expr::col((r.clone(), readings::Column::Time)),
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

/// Deployments at a site whose slot overlaps the window, optionally confined to some parameters.
///
/// An open deployment has no `deployed_until` and covers everything after its start, which is what
/// the NULL arm says.
fn deployments_over_window(
    d: &Alias,
    site_id: Uuid,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    parameter_ids: Option<&[Uuid]>,
) -> Condition {
    let mut overlapping = Condition::all()
        .add(Expr::col((d.clone(), sensor_deployments::Column::SiteId)).eq(site_id))
        .add(Expr::col((d.clone(), sensor_deployments::Column::DeployedFrom)).lt(end))
        .add(
            Condition::any()
                .add(Expr::col((d.clone(), sensor_deployments::Column::DeployedUntil)).is_null())
                .add(Expr::col((d.clone(), sensor_deployments::Column::DeployedUntil)).gt(start)),
        );
    if let Some(ids) = parameter_ids.filter(|ids| !ids.is_empty()) {
        overlapping = overlapping.add(
            Expr::col((d.clone(), sensor_deployments::Column::ParameterId)).is_in(ids.to_vec()),
        );
    }
    overlapping
}

/// The deployment bands a site's chart draws, each naming the instrument that held the slot.
pub(super) fn sensor_identity_bands_query(
    site_id: Uuid,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    parameter_ids: Option<&[Uuid]>,
) -> SelectStatement {
    let d = Alias::new("d");
    let s = Alias::new("s");
    let mut bands = SeaQuery::select();
    bands
        .expr_as(
            Expr::col((d.clone(), sensor_deployments::Column::Id)),
            Alias::new("deployment_id"),
        )
        .column((d.clone(), sensor_deployments::Column::SensorId))
        .expr_as(
            Expr::col((s.clone(), sensors::Column::SerialNumber)),
            Alias::new("sensor_serial"),
        )
        .expr_as(
            Expr::col((s.clone(), sensors::Column::Name)),
            Alias::new("sensor_name"),
        )
        .column((d.clone(), sensor_deployments::Column::SiteId))
        .column((d.clone(), sensor_deployments::Column::ParameterId))
        .column((d.clone(), sensor_deployments::Column::DeployedFrom))
        .column((d.clone(), sensor_deployments::Column::DeployedUntil))
        .from_as(sensor_deployments::Entity, d.clone())
        .join_as(
            JoinType::InnerJoin,
            sensors::Entity,
            s.clone(),
            Expr::col((s, sensors::Column::Id))
                .equals((d.clone(), sensor_deployments::Column::SensorId)),
        )
        .cond_where(deployments_over_window(
            &d,
            site_id,
            start,
            end,
            parameter_ids,
        ))
        .order_by(
            (d.clone(), sensor_deployments::Column::ParameterId),
            Order::Asc,
        )
        .order_by((d, sensor_deployments::Column::DeployedFrom), Order::Asc);
    bands
}

/// The calibration markers those bands carry: curves of the instruments deployed at the site over
/// the window, overlapping it themselves.
///
/// A marker sits on the series of the calibration's own parameter, so a curve whose parameter is
/// not resolved yet has no series to sit on and is left out.
pub(super) fn sensor_calibration_markers_query(
    site_id: Uuid,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    parameter_ids: Option<&[Uuid]>,
) -> SelectStatement {
    let c = Alias::new("c");
    let d = Alias::new("d");

    let mut deployed_here = SeaQuery::select();
    deployed_here
        .distinct()
        .column((d.clone(), sensor_deployments::Column::SensorId))
        .from_as(sensor_deployments::Entity, d.clone())
        .cond_where(deployments_over_window(
            &d,
            site_id,
            start,
            end,
            parameter_ids,
        ));

    let mut markers = SeaQuery::select();
    markers
        .expr_as(
            Expr::col((c.clone(), sensor_calibrations::Column::Id)),
            Alias::new("calibration_id"),
        )
        .column((c.clone(), sensor_calibrations::Column::SensorId))
        .column((c.clone(), sensor_calibrations::Column::ParameterId))
        .column((c.clone(), sensor_calibrations::Column::Slope))
        .column((c.clone(), sensor_calibrations::Column::Intercept))
        .column((c.clone(), sensor_calibrations::Column::ValidFrom))
        .column((c.clone(), sensor_calibrations::Column::ValidUntil))
        .from_as(sensor_calibrations::Entity, c.clone())
        .cond_where(
            Condition::all()
                .add(
                    Expr::col((c.clone(), sensor_calibrations::Column::SensorId))
                        .in_subquery(deployed_here.take()),
                )
                .add(Expr::col((c.clone(), sensor_calibrations::Column::ParameterId)).is_not_null())
                .add(Expr::col((c.clone(), sensor_calibrations::Column::ValidFrom)).lt(end))
                .add(
                    Condition::any()
                        .add(
                            Expr::col((c.clone(), sensor_calibrations::Column::ValidUntil))
                                .is_null(),
                        )
                        .add(
                            Expr::col((c.clone(), sensor_calibrations::Column::ValidUntil))
                                .gt(start),
                        ),
                ),
        )
        .order_by(
            (c.clone(), sensor_calibrations::Column::ParameterId),
            Order::Asc,
        )
        .order_by((c, sensor_calibrations::Column::ValidFrom), Order::Asc);
    markers
}

pub(super) fn parse_uuid_csv(s: &str) -> Vec<Uuid> {
    s.split(',')
        .filter_map(|p| Uuid::parse_str(p.trim()).ok())
        .collect()
}

#[cfg(test)]
#[path = "tests/service.rs"]
mod tests;
