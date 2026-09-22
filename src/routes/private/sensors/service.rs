//! Instrument queries: enrichment, stream-to-instrument identity, and the resolution the
//! pairing paths and the handlers read.

use chrono::{DateTime, Utc};
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::sea_query::{
    Alias, Asterisk, Expr, ExprTrait, Func, JoinType, OnConflict, Order, PostgresQueryBuilder,
    Query, SelectStatement,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, EntityTrait,
    FromQueryResult, QueryFilter, QueryOrder, QuerySelect, Set, Statement, TransactionTrait,
};
use std::collections::HashMap;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::routes::private::readings::models as readings;
use crate::routes::private::readings::service as readings_service;
use crate::routes::private::sensor_calibrations;
use crate::routes::private::sensor_calibrations::models as calibrations_model;
use crate::routes::private::sensor_deployments as deployments;
use crate::routes::private::sensor_deployments::models as deployments_model;
use crate::routes::private::standard_curves::models as standard_curves;
use crate::routes::private::sync::models::HoldKind;
use crate::routes::private::sync::models::HoldStatus;
use crate::routes::private::sync::service as audit;
use crate::routes::private::{data_streams, sensors};

use super::models::*;

pub struct SensorOperations;

impl CRUDOperations for SensorOperations {
    type Resource = Sensor;

    /// Mirrors `/sensors/retag_frequency`: a sensor reaching a replicate-family stream cannot be
    /// classified high-frequency through entity CRUD either, so the two routes to the same column
    /// hold the same rule.
    async fn before_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
        data: &<Sensor as CRUDResource>::UpdateModel,
    ) -> Result<(), crudcrate::ApiError> {
        if data.data_frequency.as_ref().and_then(|v| v.as_deref()) == Some("high") {
            let families =
                crate::routes::private::data_streams::service::family_keys_for_sensors(db, &[id])
                    .await
                    .map_err(|e| crudcrate::ApiError::internal(e.to_string(), None))?;
            crate::routes::private::data_streams::service::refuse_family_retag(
                &families,
                river_data_core::models::MeasurementType::Continuous.as_str(),
            )
            .map_err(|e| crudcrate::ApiError::bad_request(e.to_string()))?;
        }
        Ok(())
    }

    /// The dependent tables all reference sensors without ON DELETE, so the constraint would
    /// refuse this anyway; the check turns that into a stated 400 instead of an internal error.
    async fn before_delete<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
    ) -> Result<(), ApiError> {
        let blocking = db
            .query_one_raw(holders_of_instrument(id))
            .await
            .map_err(ApiError::database)?;
        if let Some(row) = blocking {
            let mut held: Vec<&str> = Vec::new();
            let blocking = BlockingRow::from_query_result(&row, "").map_err(ApiError::database)?;
            for (present, label) in [
                (blocking.readings, "readings"),
                (blocking.curves, "standard curves"),
                (blocking.calibrations, "calibrations"),
                (blocking.deployments, "deployments"),
            ] {
                if present {
                    held.push(label);
                }
            }
            if !held.is_empty() {
                return Err(ApiError::bad_request(format!(
                    "Sensor {id} still holds {}. Remove or reassign them first; readings and \
                     applied curves are provenance and keep the instrument on record.",
                    held.join(", ")
                )));
            }
        }
        Ok(())
    }

    async fn after_get_one<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut Sensor,
    ) -> Result<(), ApiError> {
        let mut enriched = enrich(db, &[entity.id]).await?;
        if let Some(fields) = enriched.remove(&entity.id) {
            fields.apply(entity);
        }
        Ok(())
    }

    async fn after_get_all<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entities: &mut Vec<<Sensor as CRUDResource>::ListModel>,
    ) -> Result<(), ApiError> {
        if entities.is_empty() {
            return Ok(());
        }
        let ids: Vec<Uuid> = entities.iter().map(|e| e.id).collect();
        let enriched = enrich(db, &ids).await?;
        for entity in entities.iter_mut() {
            if let Some(fields) = enriched.get(&entity.id) {
                fields.clone().apply(entity);
            }
        }
        Ok(())
    }
}

/// What both sensor read paths report over the stored row: where a device is, when it was last
/// calibrated, what it last measured, and what a lab instrument has instead of a deployment. One
/// shape, so the detail and the list cannot disagree about a sensor.
#[derive(Clone, Debug, Default)]
struct Enrichment {
    current_site: Option<(Uuid, String)>,
    last_calibration_at: Option<DateTime<Utc>>,
    last_reading_at: Option<DateTime<Utc>>,
    last_reading_value: Option<f64>,
    reading_count: Option<i64>,
    curve_count: Option<i64>,
    last_curve_use: Option<DateTime<Utc>>,
}

/// The read models this enrichment is written onto. `Sensor` and its `ListModel` are separate
/// generated types carrying the same fields, so one trait is what lets the write be stated once.
trait Enriched {
    fn current_site_id(&mut self) -> &mut Option<Uuid>;
    fn current_site_name(&mut self) -> &mut Option<String>;
    fn last_calibration_at(&mut self) -> &mut Option<DateTime<Utc>>;
    fn last_reading_at(&mut self) -> &mut Option<DateTime<Utc>>;
    fn last_reading_value(&mut self) -> &mut Option<f64>;
    fn reading_count(&mut self) -> &mut Option<i64>;
    fn curve_count(&mut self) -> &mut Option<i64>;
    fn last_curve_use(&mut self) -> &mut Option<DateTime<Utc>>;
}

macro_rules! enriched {
    ($t:ty) => {
        impl Enriched for $t {
            fn current_site_id(&mut self) -> &mut Option<Uuid> {
                &mut self.current_site_id
            }
            fn current_site_name(&mut self) -> &mut Option<String> {
                &mut self.current_site_name
            }
            fn last_calibration_at(&mut self) -> &mut Option<DateTime<Utc>> {
                &mut self.last_calibration_at
            }
            fn last_reading_at(&mut self) -> &mut Option<DateTime<Utc>> {
                &mut self.last_reading_at
            }
            fn last_reading_value(&mut self) -> &mut Option<f64> {
                &mut self.last_reading_value
            }
            fn reading_count(&mut self) -> &mut Option<i64> {
                &mut self.reading_count
            }
            fn curve_count(&mut self) -> &mut Option<i64> {
                &mut self.curve_count
            }
            fn last_curve_use(&mut self) -> &mut Option<DateTime<Utc>> {
                &mut self.last_curve_use
            }
        }
    };
}

enriched!(Sensor);

enriched!(<Sensor as CRUDResource>::ListModel);

impl Enrichment {
    /// Write what was resolved, leaving what was not exactly as the row had it: an absent fact is
    /// not a fact of absence.
    fn apply(self, entity: &mut impl Enriched) {
        if let Some((site_id, site_name)) = self.current_site {
            *entity.current_site_id() = Some(site_id);
            *entity.current_site_name() = Some(site_name);
        }
        if self.last_calibration_at.is_some() {
            *entity.last_calibration_at() = self.last_calibration_at;
        }
        if self.last_reading_at.is_some() {
            *entity.last_reading_at() = self.last_reading_at;
        }
        if self.last_reading_value.is_some() {
            *entity.last_reading_value() = self.last_reading_value;
        }
        if self.reading_count.is_some() {
            *entity.reading_count() = self.reading_count;
        }
        if self.curve_count.is_some() {
            *entity.curve_count() = self.curve_count;
        }
        if self.last_curve_use.is_some() {
            *entity.last_curve_use() = self.last_curve_use;
        }
    }
}

/// Resolve [`Enrichment`] for a set of sensors in a fixed number of queries, whatever the set's
/// size.
///
/// Summaries instead of an unbounded readings scan, whose planning cost grows with the
/// hypertable's chunk count: the count is the hourly rollup's population plus recent spot rows,
/// and the newest instant is the stream ingest cursor with the rollup's newest bucket as fallback.
async fn enrich<C: ConnectionTrait>(
    db: &C,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Enrichment>, ApiError> {
    let mut out: HashMap<Uuid, Enrichment> = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let values = [ids.to_vec().into()];

    // Where the instrument is now.
    let dep_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT sd.sensor_id, sd.site_id, s.name AS site_name
                  FROM sensor_deployments sd JOIN sites s ON s.id = sd.site_id
                  WHERE sd.sensor_id = ANY($1) AND sd.deployed_until IS NULL",
            values.clone(),
        ))
        .await
        .map_err(ApiError::database)?;
    for row in &dep_rows {
        if let Ok(row) = OpenDeploymentRow::from_query_result(row, "") {
            let entry = out.entry(row.sensor_id).or_default();
            if entry.current_site.is_none() {
                entry.current_site = Some((row.site_id, row.site_name));
            }
        }
    }

    // When it was last calibrated.
    let cal_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT DISTINCT ON (sensor_id) sensor_id, valid_from
                  FROM sensor_calibrations
                  WHERE sensor_id = ANY($1)
                  ORDER BY sensor_id, valid_from DESC",
            values.clone(),
        ))
        .await
        .map_err(ApiError::database)?;
    for row in &cal_rows {
        if let Ok(row) = LastCalibrationRow::from_query_result(row, "") {
            out.entry(row.sensor_id).or_default().last_calibration_at =
                Some(row.valid_from.with_timezone(&Utc));
        }
    }

    // How much it has measured: the rollup's population plus the spot rows it does not carry.
    let count_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sensor_id, COALESCE(SUM(count), 0)::bigint AS n FROM readings_hourly \
                  WHERE sensor_id = ANY($1) GROUP BY sensor_id",
            values.clone(),
        ))
        .await
        .map_err(ApiError::database)?;
    for row in &count_rows {
        if let Ok(row) = CountRow::from_query_result(row, "") {
            let entry = out.entry(row.sensor_id).or_default();
            entry.reading_count = Some(entry.reading_count.unwrap_or(0) + row.n);
        }
    }
    let spot_rows = db
        .query_all_raw(recent_spot_counts(ids))
        .await
        .map_err(ApiError::database)?;
    for row in &spot_rows {
        if let Ok(row) = CountRow::from_query_result(row, "") {
            let entry = out.entry(row.sensor_id).or_default();
            entry.reading_count = Some(entry.reading_count.unwrap_or(0) + row.n);
        }
    }
    for id in ids {
        out.entry(*id).or_default().reading_count.get_or_insert(0);
    }

    // What it last measured. The instant comes from the ingest cursors and the rollup; only the
    // value is read from `readings`, over a window those two bound, so chunk exclusion applies.
    let cursor_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT sensor_id, MAX(last_data_time) AS last_time
                  FROM data_streams
                  WHERE sensor_id = ANY($1) AND last_data_time IS NOT NULL
                  GROUP BY sensor_id",
            values.clone(),
        ))
        .await
        .map_err(ApiError::database)?;
    let bucket_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sensor_id, MAX(bucket) AS last_bucket FROM readings_hourly \
                  WHERE sensor_id = ANY($1) GROUP BY sensor_id",
            values,
        ))
        .await
        .map_err(ApiError::database)?;

    // `newest` is the instant to report when no value is found; `window_end` bounds the lookup,
    // and a bucket's own rows lie inside it, so it is the bucket's end.
    let mut newest: HashMap<Uuid, DateTime<Utc>> = HashMap::new();
    let mut window_end: HashMap<Uuid, DateTime<Utc>> = HashMap::new();
    let note = |map: &mut HashMap<Uuid, DateTime<Utc>>, id: Uuid, t: DateTime<Utc>| {
        map.entry(id)
            .and_modify(|cur| *cur = Ord::max(*cur, t))
            .or_insert(t);
    };
    for row in &cursor_rows {
        if let Ok(row) = LastTimeRow::from_query_result(row, "") {
            let t = row.last_time.with_timezone(&Utc);
            note(&mut newest, row.sensor_id, t);
            note(&mut window_end, row.sensor_id, t);
        }
    }
    for row in &bucket_rows {
        if let Ok(row) = LastBucketRow::from_query_result(row, "") {
            let bucket = row.last_bucket.with_timezone(&Utc);
            note(&mut newest, row.sensor_id, bucket);
            note(
                &mut window_end,
                row.sensor_id,
                bucket + chrono::Duration::hours(1),
            );
        }
    }
    for (id, t) in &newest {
        out.entry(*id).or_default().last_reading_at = Some(*t);
    }

    // A sensor neither the cursors nor the rollup cover (spot-only, never ingested) gets one
    // bounded probe of its own rather than widening everyone else's window.
    let uncovered: Vec<Uuid> = ids
        .iter()
        .filter(|id| !window_end.contains_key(id))
        .copied()
        .collect();
    if !uncovered.is_empty() {
        let rows = db
            .query_all_raw(newest_value_per_instrument(&uncovered, None))
            .await
            .map_err(ApiError::database)?;
        record_values(&rows, &mut out);
    }

    // Sensors whose windows lie within 30 days of each other share one query; a stale straggler
    // gets its own rather than widening the cluster's.
    let mut windows: Vec<(Uuid, DateTime<Utc>)> = window_end.into_iter().collect();
    windows.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
    let mut i = 0;
    while i < windows.len() {
        let hi = windows[i].1;
        let mut lo = windows[i].1;
        let mut cluster: Vec<Uuid> = Vec::new();
        while i < windows.len() && hi - windows[i].1 <= chrono::Duration::days(30) {
            lo = windows[i].1;
            cluster.push(windows[i].0);
            i += 1;
        }
        let rows = db
            .query_all_raw(newest_value_per_instrument(
                &cluster,
                Some((lo - chrono::Duration::days(1), hi)),
            ))
            .await
            .map_err(ApiError::database)?;
        record_values(&rows, &mut out);
    }

    // What a lab instrument has where a device has a deployment: the curves fitted on it, and the
    // newest reading any of them corrected. One grouped pass over the partial index on
    // `standard_curve_id`, which a small fraction of readings carry.
    // Every requested instrument gets a count, so no instrument reports "no curves" as "unknown".
    for id in ids {
        out.entry(*id).or_default().curve_count = Some(0);
    }
    let curve_rows = db
        .query_all_raw(curve_use_per_instrument(ids))
        .await
        .map_err(ApiError::database)?;
    for row in &curve_rows {
        let Ok(row) = CurveUseRow::from_query_result(row, "") else {
            continue;
        };
        let entry = out.entry(row.sensor_id).or_default();
        entry.curve_count = Some(row.curves);
        entry.last_curve_use = row.last_use.map(|t| t.with_timezone(&Utc));
    }

    Ok(out)
}

/// What still points at an instrument. One row of four booleans, so a refusal names everything it
/// holds rather than the first thing found.
fn holders_of_instrument(id: Uuid) -> Statement {
    let query = Query::select()
        .expr_as(
            holds(readings::Entity, readings::Column::SensorId, id),
            Alias::new("readings"),
        )
        .expr_as(
            holds(
                standard_curves::Entity,
                standard_curves::Column::SensorId,
                id,
            ),
            Alias::new("curves"),
        )
        .expr_as(
            holds(
                calibrations_model::Entity,
                calibrations_model::Column::SensorId,
                id,
            ),
            Alias::new("calibrations"),
        )
        .expr_as(
            holds(
                deployments_model::Entity,
                deployments_model::Column::SensorId,
                id,
            ),
            Alias::new("deployments"),
        )
        .to_owned();
    build(&query)
}

/// Whether any row of `entity` names this instrument.
fn holds<E: EntityTrait>(entity: E, column: E::Column, id: Uuid) -> Expr {
    Expr::exists(
        Query::select()
            .expr(Expr::val(1))
            .from(entity)
            .and_where(Expr::col(column).eq(id))
            .to_owned(),
    )
}

/// How many unflagged spot replicates each instrument produced in the last 90 days.
fn recent_spot_counts(ids: &[Uuid]) -> Statement {
    let query = Query::select()
        .column(readings::Column::SensorId)
        .expr_as(Expr::col(Asterisk).count(), Alias::new("n"))
        .from(readings::Entity)
        .and_where(readings::Column::SensorId.is_in(ids.to_vec()))
        .and_where(Expr::cust("time > now() - INTERVAL '90 days'"))
        .and_where(readings::Column::MeasurementType.eq(readings_service::SPOT))
        .and_where(Expr::cust("is_flagged IS NOT TRUE"))
        .add_group_by([Expr::col(readings::Column::SensorId)])
        .to_owned();
    build(&query)
}

/// The newest value each instrument measured, over the given window or the last 90 days. The
/// window is what keeps the index scan bounded; `DISTINCT ON` takes the first row per instrument.
fn newest_value_per_instrument(
    ids: &[Uuid],
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Statement {
    let mut query = Query::select();
    query
        .distinct_on([readings::Column::SensorId])
        .column(readings::Column::SensorId)
        .column(readings::Column::Time)
        .expr_as(
            Func::coalesce([
                Expr::col(readings::Column::CalibratedValue),
                Expr::col(readings::Column::RawValue),
            ]),
            Alias::new("value"),
        )
        .from(readings::Entity)
        .and_where(readings::Column::SensorId.is_in(ids.to_vec()))
        .order_by(readings::Column::SensorId, Order::Asc)
        .order_by(readings::Column::Time, Order::Desc);
    match window {
        Some((from, to)) => {
            query
                .and_where(readings::Column::Time.gte(from))
                .and_where(readings::Column::Time.lte(to));
        }
        None => {
            query.and_where(Expr::cust("time > now() - INTERVAL '90 days'"));
        }
    }
    build(&query)
}

/// The curves each lab instrument fitted, and the newest reading any of them corrected.
fn curve_use_per_instrument(ids: &[Uuid]) -> Statement {
    let sc = Alias::new("sc");
    let r = Alias::new("r");
    let query = Query::select()
        .column((sc.clone(), standard_curves::Column::SensorId))
        .expr_as(
            Expr::col((sc.clone(), standard_curves::Column::Id)).count_distinct(),
            Alias::new("curves"),
        )
        .expr_as(
            Expr::col((r.clone(), readings::Column::Time)).max(),
            Alias::new("last_use"),
        )
        .from_as(standard_curves::Entity, sc.clone())
        .join_as(
            JoinType::LeftJoin,
            readings::Entity,
            r.clone(),
            Expr::col((r, readings::Column::StandardCurveId))
                .equals((sc.clone(), standard_curves::Column::Id)),
        )
        .and_where(Expr::col((sc.clone(), standard_curves::Column::SensorId)).is_in(ids.to_vec()))
        .add_group_by([Expr::col((sc, standard_curves::Column::SensorId))])
        .to_owned();
    build(&query)
}

fn build(query: &SelectStatement) -> Statement {
    let (sql, values) = query.build(PostgresQueryBuilder);
    Statement::from_sql_and_values(sea_orm::DatabaseBackend::Postgres, sql, values)
}

/// What still points at an instrument, which is what refuses its deletion.
#[derive(FromQueryResult)]
struct BlockingRow {
    readings: bool,
    curves: bool,
    calibrations: bool,
    deployments: bool,
}

#[derive(FromQueryResult)]
struct OpenDeploymentRow {
    sensor_id: Uuid,
    site_id: Uuid,
    site_name: String,
}

#[derive(FromQueryResult)]
struct LastCalibrationRow {
    sensor_id: Uuid,
    valid_from: DateTime<chrono::FixedOffset>,
}

/// A per-instrument tally, from the rollup and from the spot rows alike.
#[derive(FromQueryResult)]
struct CountRow {
    sensor_id: Uuid,
    n: i64,
}

#[derive(FromQueryResult)]
struct LastTimeRow {
    sensor_id: Uuid,
    last_time: DateTime<chrono::FixedOffset>,
}

#[derive(FromQueryResult)]
struct LastBucketRow {
    sensor_id: Uuid,
    last_bucket: DateTime<chrono::FixedOffset>,
}

#[derive(FromQueryResult)]
struct CurveUseRow {
    sensor_id: Uuid,
    curves: i64,
    last_use: Option<sea_orm::prelude::DateTimeWithTimeZone>,
}

#[derive(FromQueryResult)]
struct ProbedRow {
    sensor_id: Uuid,
    time: DateTime<chrono::FixedOffset>,
    value: f64,
}

/// The instant and value of each probed row, over whatever the enrichment already holds.
fn record_values(rows: &[sea_orm::QueryResult], out: &mut HashMap<Uuid, Enrichment>) {
    for row in rows {
        if let Ok(row) = ProbedRow::from_query_result(row, "") {
            let entry = out.entry(row.sensor_id).or_default();
            entry.last_reading_at = Some(row.time.with_timezone(&Utc));
            entry.last_reading_value = Some(row.value);
        }
    }
}

/// The source device identity a stream reports, as it is recorded on the sensor. Public so the
/// registration path can compare what a feed now reports against what its instrument was minted
/// with.
#[must_use]
pub fn source_identity(stream_metadata: &serde_json::Value) -> Option<serde_json::Value> {
    extract_source_metadata(stream_metadata)
}

/// Extract Vaisala device metadata from stream metadata for storage on the sensor.
fn extract_source_metadata(stream_metadata: &serde_json::Value) -> Option<serde_json::Value> {
    let device = stream_metadata.get("device")?;
    let mut meta = serde_json::Map::new();

    if let Some(v) = device.get("logger_serial").and_then(|v| v.as_str())
        && !v.is_empty()
    {
        meta.insert(
            "source_device_serial".to_string(),
            serde_json::Value::String(v.to_string()),
        );
    }
    if let Some(v) = device.get("probe_serial").and_then(|v| v.as_str())
        && !v.is_empty()
    {
        meta.insert(
            "source_probe_serial".to_string(),
            serde_json::Value::String(v.to_string()),
        );
    }
    if let Some(v) = device.get("logger_device").and_then(|v| v.as_str())
        && !v.is_empty()
    {
        meta.insert(
            "source_device_model".to_string(),
            serde_json::Value::String(v.to_string()),
        );
    }
    if let Some(v) = device.get("device_class").and_then(|v| v.as_str())
        && !v.is_empty()
    {
        meta.insert(
            "source_device_class".to_string(),
            serde_json::Value::String(v.to_string()),
        );
    }

    if meta.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(meta))
    }
}

/// Find an existing source-registered instrument by its natural key `(source_system, source_key)`.
/// The channel is the identity: a device instrument's `source_key` is its stream's `source_key`,
/// so a multi-channel logger resolves to one instrument per channel rather than one per device.
async fn find_sensor_by_source<C: ConnectionTrait>(
    db: &C,
    source_system: &str,
    source_key: &str,
) -> AppResult<Option<sensors::Model>> {
    let existing = sensors::Entity::find()
        .filter(sensors::Column::SourceSystem.eq(source_system))
        .filter(sensors::Column::SourceKey.eq(source_key))
        .one(db)
        .await?;
    Ok(existing)
}

/// Link a data stream to a sensor (`data_streams.sensor_id`) as the pairing hint.
async fn link_stream_to_sensor<C: ConnectionTrait>(
    db: &C,
    stream: &data_streams::Model,
    sensor_id: Uuid,
) -> AppResult<()> {
    if stream.sensor_id == Some(sensor_id) {
        return Ok(());
    }
    let mut stream_active: data_streams::ActiveModel = stream.clone().into();
    stream_active.sensor_id = Set(Some(sensor_id));
    stream_active.updated_at = Set(Utc::now().into());
    stream_active.update(db).await?;
    Ok(())
}

#[derive(Debug, FromQueryResult)]
struct SlotNamesRow {
    site_name: String,
    parameter_name: String,
}

#[derive(Debug, FromQueryResult)]
struct FirstReadingRow {
    first_reading: Option<DateTime<Utc>>,
}

/// Why a write may not name this instrument, or `None` when it may. A bookkeeping row records
/// that nothing was declared, and a retired one is no longer in the lab; an unset `is_active`
/// is the column's default, which is active.
#[must_use]
pub fn instrument_refusal(
    kind: InstrumentKind,
    is_active: Option<bool>,
    name: &str,
    subject: &str,
) -> Option<String> {
    if kind.is_bookkeeping() {
        return Some(format!(
            "{name} is a {} row, which records that nothing was declared; it cannot be {subject}",
            kind.as_str()
        ));
    }
    if !is_active.unwrap_or(true) {
        return Some(format!(
            "{name} is retired, so it cannot be {subject}; reactivate the instrument first"
        ));
    }
    None
}

/// The cadence a slot this instrument is adopted into is filled at: the instrument's own
/// `data_frequency`, which already speaks `high|low`. An instrument the register has lost falls
/// back to `high`, the cadence a slot declares when nobody declares one.
pub async fn cadence_of<C: ConnectionTrait>(db: &C, sensor_id: Uuid) -> AppResult<String> {
    Ok(Entity::find_by_id(sensor_id)
        .select_only()
        .column(Column::DataFrequency)
        .into_tuple::<String>()
        .one(db)
        .await?
        .unwrap_or_else(|| "high".to_string()))
}

/// The same rule over the instruments one request names, in one query. A grab save carries an
/// instrument per row, so asking per row would be a query per row on the entry path.
pub async fn require_measuring_instruments<C: ConnectionTrait>(
    db: &C,
    sensor_ids: &[Uuid],
    subject: &str,
) -> AppResult<()> {
    let mut wanted: Vec<Uuid> = sensor_ids.to_vec();
    wanted.sort_unstable();
    wanted.dedup();
    if wanted.is_empty() {
        return Ok(());
    }
    let rows = Entity::find()
        .filter(Column::Id.is_in(wanted.clone()))
        .select_only()
        .column(Column::Id)
        .column(Column::Kind)
        .column(Column::Name)
        .column(Column::IsLabInstrument)
        .column(Column::IsActive)
        .into_tuple::<(
            Uuid,
            Option<String>,
            Option<String>,
            Option<bool>,
            Option<bool>,
        )>()
        .all(db)
        .await?;
    for id in &wanted {
        let Some((_, stored_kind, name, is_lab_instrument, is_active)) =
            rows.iter().find(|r| r.0 == *id)
        else {
            return Err(AppError::BadRequest(format!("Instrument {id} not found")));
        };
        let kind = InstrumentKind::of(stored_kind.as_deref(), *is_lab_instrument);
        let name = name.clone().unwrap_or_else(|| id.to_string());
        if let Some(message) = instrument_refusal(kind, *is_active, &name, subject) {
            return Err(AppError::BadRequest(message));
        }
    }
    Ok(())
}

/// Refuse an instrument nothing measures on, and one that has been retired, for the writes that
/// name one: a slot's declaration and a deployment. `subject` names the write in the message,
/// since the operator picked the row from a list and needs to be told why this one is not an
/// answer.
pub async fn require_measuring_instrument<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
    subject: &str,
) -> AppResult<()> {
    let (stored_kind, name, is_lab_instrument, is_active) = Entity::find_by_id(sensor_id)
        .select_only()
        .column(Column::Kind)
        .column(Column::Name)
        .column(Column::IsLabInstrument)
        .column(Column::IsActive)
        .into_tuple::<(Option<String>, Option<String>, Option<bool>, Option<bool>)>()
        .one(db)
        .await?
        .ok_or_else(|| AppError::BadRequest(format!("Instrument {sensor_id} not found")))?;
    let kind = InstrumentKind::of(stored_kind.as_deref(), is_lab_instrument);
    let name = name.unwrap_or_else(|| sensor_id.to_string());
    match instrument_refusal(kind, is_active, &name, subject) {
        Some(message) => Err(AppError::BadRequest(message)),
        None => Ok(()),
    }
}

/// Insert a source-registered instrument for `(source_system, source_key)`, or return the existing
/// one. Race-safe: the `ON CONFLICT … DO NOTHING` targets the partial unique index
/// `sensors_provenance_uniq (source_system, source_key)`, so concurrent pairings of the same
/// channel converge on one row WITHOUT raising a unique violation. That matters because some
/// callers run inside a transaction (sync plan/discovery apply): a raised violation there would
/// poison the whole transaction, not just this insert. The conflict branch re-selects the winner.
///
/// Both key halves are required, which is what makes minting idempotent: there is no "no dedupe
/// key" case that inserts a fresh row on every call.
///
/// `DO NOTHING` rather than `DO UPDATE`: refreshing a claimed instrument's metadata belongs to
/// [`reconcile_source_identity`], which raises a review hold, so a sync cycle cannot silently
/// overwrite what an operator recorded.
pub async fn upsert_source_instrument<C: ConnectionTrait>(
    db: &C,
    source_system: &str,
    source_key: &str,
    name: &str,
    kind: InstrumentKind,
    data_frequency: &str,
    metadata: Option<serde_json::Value>,
) -> AppResult<Uuid> {
    use sea_orm::sea_query::ExprTrait;
    let mint = ActiveModel {
        id: Set(Uuid::new_v4()),
        name: Set(Some(name.to_string())),
        is_active: Set(Some(true)),
        is_lab_instrument: Set(Some(kind.is_lab_instrument())),
        data_frequency: Set(data_frequency.to_string()),
        source_system: Set(Some(source_system.to_string())),
        source_key: Set(Some(source_key.to_string())),
        metadata: Set(metadata),
        kind: Set(kind.as_str().to_string()),
        created_at: Set(Some(Utc::now())),
        ..Default::default()
    };
    // The unique index is partial, so the conflict target carries its predicate or Postgres
    // cannot infer which index the arm names.
    let inserted = Entity::insert(mint)
        .on_conflict(
            OnConflict::columns([Column::SourceSystem, Column::SourceKey])
                .target_and_where(
                    Expr::col(Column::SourceSystem)
                        .is_not_null()
                        .and(Expr::col(Column::SourceKey).is_not_null()),
                )
                .do_nothing()
                .to_owned(),
        )
        .exec_without_returning(db)
        .await?;

    if inserted > 0 {
        return find_sensor_by_source(db, source_system, source_key)
            .await?
            .map(|row| row.id)
            .ok_or_else(|| {
                AppError::Internal("the instrument just minted was not found".to_string())
            });
    }

    let existing = find_sensor_by_source(db, source_system, source_key)
        .await?
        .ok_or_else(|| {
            AppError::Internal(
                "instrument upsert conflicted but the existing provenance row was not found"
                    .to_string(),
            )
        })?;
    Ok(existing.id)
}

/// The instrument a stream's readings are attributed to, resolved or minted.
///
/// Every stream has one. A measurement whose instrument is unknown is a measurement whose
/// provenance cannot be recovered later, so no write path may leave `sensor_id` NULL, and the
/// resolution below always ends in an id.
///
/// Identity, most specific first:
/// 1. the instrument the stream already names,
/// 2. a device feed's own channel `(source_system, source_key)`, one field instrument per channel,
///    which is what the viewLinc backend writes and where the probe is the instrument,
/// 3. the source's instrument for the parameter the feed carries, `{source_system}:{parameter}`:
///    one lab instrument per parameter across every station, which is the key the pairing plan
///    mints and resolves under, so a hand pairing and a plan converge on the same row.
///
/// `name_hint` names a device channel or a hand-entry channel. A source-parameter or lab
/// instrument is named for its parameter and source whatever the hint says, since it serves every
/// station that reports the parameter.
///
/// Updates `data_streams.sensor_id` to link the stream.
pub async fn resolve_or_mint_stream_instrument<C: ConnectionTrait>(
    db: &C,
    stream: &data_streams::Model,
    name_hint: Option<&str>,
    kind: InstrumentKind,
) -> AppResult<Uuid> {
    if let Some(sensor_id) = stream.sensor_id {
        return Ok(sensor_id);
    }
    if is_per_site_instrument(&stream.metadata) {
        return Ok(import_sensor_for_stream(db, stream, name_hint)
            .await?
            .sensor_id);
    }

    let source_key = crate::routes::private::sync::service::stream_instrument_key(stream);
    let key_part = source_key
        .strip_prefix(&format!("{}:", stream.source_system))
        .unwrap_or(&source_key)
        .to_string();
    let name = source_instrument_name(kind, &key_part, &stream.source_system, name_hint);
    let sensor_id = upsert_source_instrument(
        db,
        &stream.source_system,
        &source_key,
        &name,
        kind,
        // A bookkeeping instrument carries no evidence about cadence, and `data_frequency` is
        // read as one by `resolve_measurement_type`. 'high' leaves that rung silent, so only a
        // declaration or a real device moves a stream to spot.
        "high",
        Some(serde_json::json!({ super::models::MINTED_FROM_STREAM: stream.source_key })),
    )
    .await?;
    link_stream_to_sensor(db, stream, sensor_id).await?;
    Ok(sensor_id)
}

/// Create or reuse the instrument for a data stream being paired, and deploy it when it is a field
/// instrument.
///
/// The instrument comes from [`resolve_or_mint_stream_instrument`], so a stream reaches its slot
/// attributed whatever its source is. A lab instrument gets no deployment: it corrects a grab, it
/// is not stationed at the site, which is the "attributed but not deployed" state
/// `import_sensor_for_stream` documents. No calibration is created either; the context carries
/// whichever curve the instrument already has.
///
/// `name` is a device channel's name as a pairing plan confirmed it; without one the channel is
/// named for the slot.
pub async fn create_sensor_for_stream<C: ConnectionTrait>(
    db: &C,
    stream: &data_streams::Model,
    parameter_id: Uuid,
    site_id: Uuid,
    name: Option<&str>,
) -> AppResult<SensorContext> {
    let name = match name {
        Some(name) => Some(name.to_string()),
        None => slot_instrument_name(db, site_id, parameter_id).await?,
    };
    let sensor_id = resolve_or_mint_stream_instrument(
        db,
        stream,
        name.as_deref(),
        InstrumentKind::SourceParameter,
    )
    .await?;
    let is_lab = sensors::Entity::find_by_id(sensor_id)
        .one(db)
        .await?
        .is_some_and(|s| s.is_lab_instrument.unwrap_or(false));
    // Ensure active deployment exists for this sensor+site+parameter (None if the slot is occupied).
    let deployment_id = if is_lab {
        None
    } else {
        find_or_create_deployment(
            db,
            sensor_id,
            site_id,
            parameter_id,
            stream_history_start(db, stream.id).await?,
        )
        .await?
    };
    Ok(SensorContext {
        sensor_id,
        deployment_id,
    })
}

/// The instrument a per-slot internal channel (grab entry, API batch) attributes its readings to.
///
/// A hand-entered value is not the deployed probe's measurement, so the slot's field instrument is
/// never borrowed here: the channel takes its own instrument, named for the slot it serves and
/// minted the first time anything is entered there. An explicit instrument on the reading, the
/// lab instrument a tool run's curve names, still wins over it at the write.
pub async fn ensure_channel_instrument<C: ConnectionTrait>(
    db: &C,
    stream: &data_streams::Model,
    site_id: Uuid,
    parameter_id: Uuid,
    kind: &str,
) -> AppResult<Uuid> {
    let name = slot_instrument_name(db, site_id, parameter_id)
        .await?
        .map(|slot| format!("{slot} ({kind})"));
    resolve_or_mint_stream_instrument(db, stream, name.as_deref(), InstrumentKind::EntryChannel)
        .await
}

/// Whether a stream's instrument is its own channel, one per site and parameter, rather than the
/// source's instrument for the parameter. The connector declares it at registration; a stream
/// registered without a declaration is one when its metadata carries a `device` block.
#[must_use]
pub fn is_per_site_instrument(stream_metadata: &serde_json::Value) -> bool {
    use river_data_core::models::InstrumentGranularity;
    match crate::routes::private::data_streams::service::declared_instrument_granularity(
        stream_metadata,
    ) {
        Some(InstrumentGranularity::PerSiteParameter) => true,
        Some(InstrumentGranularity::PerParameter) => false,
        None => stream_metadata.get("device").is_some_and(|d| !d.is_null()),
    }
}

/// The name a non-device instrument takes when it is minted.
///
/// A source-parameter or lab instrument is one row for the parameter across every station that
/// reports it, so it is named for the parameter and the source; a slot name would be true of
/// whichever station registered first and of no other. A hand-entry channel is per slot and keeps
/// the name its caller gives it.
#[must_use]
fn source_instrument_name(
    kind: InstrumentKind,
    key_part: &str,
    source_system: &str,
    name_hint: Option<&str>,
) -> String {
    match kind {
        InstrumentKind::SourceParameter | InstrumentKind::Lab => None,
        _ => name_hint,
    }
    .map_or_else(
        || format!("{key_part} ({source_system})"),
        ToString::to_string,
    )
}

/// The name a source-registered field instrument takes: the slot it serves, "{site} {parameter}".
/// The migration that split logger-keyed rows into per-channel ones names them the same way, so
/// instruments minted before and after it read alike.
async fn slot_instrument_name<C: ConnectionTrait>(
    db: &C,
    site_id: Uuid,
    parameter_id: Uuid,
) -> AppResult<Option<String>> {
    let row = SlotNamesRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT s.name AS site_name, p.name AS parameter_name \
             FROM sites s, parameters p WHERE s.id = $1 AND p.id = $2",
        [site_id.into(), parameter_id.into()],
    ))
    .one(db)
    .await?;
    let Some(row) = row else { return Ok(None) };
    Ok(Some(format!("{} {}", row.site_name, row.parameter_name)))
}

/// The frequency a device feed's instrument is minted at. The stream's own declaration is the
/// evidence: a device reporting one reading per visit declares `spot`, and an undeclared stream
/// is a logger.
pub(super) fn device_frequency(measurement_type: Option<&str>) -> &'static str {
    if measurement_type == Some("spot") {
        return "low";
    }
    "high"
}

/// Create or reuse the device instrument feeding a stream, WITHOUT deploying it to a site: the
/// readings get `sensor_id` but no `deployment_id`/`site_id` until an explicit adopt. Idempotent:
/// reuses the stream's linked instrument, else mints it (race-safe via
/// [`upsert_source_instrument`]). Updates `data_streams.sensor_id`.
///
/// `name_hint` is the slot name when the caller knows which slot the stream is being paired to.
pub async fn import_sensor_for_stream<C: ConnectionTrait>(
    db: &C,
    stream: &data_streams::Model,
    name_hint: Option<&str>,
) -> AppResult<SensorContext> {
    let sensor_id = if let Some(existing_sensor_id) = stream.sensor_id {
        existing_sensor_id
    } else {
        let sensor_name = name_hint.map_or_else(
            || {
                stream
                    .source_name
                    .clone()
                    .unwrap_or_else(|| format!("Stream {}", stream.source_key))
            },
            ToString::to_string,
        );
        let metadata = extract_source_metadata(&stream.metadata);
        let sensor_id = upsert_source_instrument(
            db,
            &stream.source_system,
            &stream.source_key,
            &sensor_name,
            InstrumentKind::Device,
            device_frequency(stream.measurement_type.as_deref()),
            metadata,
        )
        .await?;
        link_stream_to_sensor(db, stream, sensor_id).await?;
        sensor_id
    };

    Ok(SensorContext {
        sensor_id,
        deployment_id: None,
    })
}

/// When a stream's history begins, which is when a deployment auto-created for it opens: a stream
/// paired months after it started measuring has readings the site is entitled to, and a deployment
/// opening at the pairing instant leaves every one of them without one. `NOW()` when the stream has
/// no readings yet.
pub async fn stream_history_start<C: ConnectionTrait>(
    db: &C,
    stream_id: Uuid,
) -> AppResult<DateTime<Utc>> {
    let first = readings::Entity::find()
        .select_only()
        .column_as(readings::Column::Time.min(), "first_reading")
        .filter(readings::Column::StreamId.eq(stream_id))
        .into_model::<FirstReadingRow>()
        .one(db)
        .await?
        .and_then(|r| r.first_reading);
    Ok(first.unwrap_or_else(Utc::now))
}

/// Find this sensor's open deployment at the site, or auto-create one, but only if the
/// `(site, parameter)` slot is free. Returns `None` when the slot is already occupied by another
/// sensor (the swap case), leaving the deployment to an explicit adopt.
///
/// One sensor per `(site, parameter)` is hard-enforced by the `excl_deployment_site_param_slot`
/// exclusion constraint. A blind insert onto an occupied slot would raise an exclusion violation,
/// which, in the sync apply path (this runs inside `create_sensor_for_stream` within a transaction),
/// would poison the whole pairing transaction. The conditional insert below skips cleanly when the
/// slot is occupied (the common swap case) instead of raising; the constraint remains the atomic
/// backstop for the rare concurrent-double-deploy race.
pub async fn find_or_create_deployment<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
    site_id: Uuid,
    parameter_id: Uuid,
    opens_at: DateTime<Utc>,
) -> AppResult<Option<Uuid>> {
    let existing = deployments::Entity::find()
        .filter(
            Condition::all()
                .add(deployments::Column::SensorId.eq(sensor_id))
                .add(deployments::Column::SiteId.eq(site_id))
                .add(deployments::Column::ParameterId.eq(parameter_id))
                .add(deployments::Column::DeployedUntil.is_null()),
        )
        .one(db)
        .await?;

    if let Some(dep) = existing {
        return Ok(Some(dep.id));
    }

    // Insert an open deployment only when nothing else holds the (site, parameter) slot open. It
    // opens at `opens_at`, clamped forward to the end of the last deployment that covered the slot:
    // that instrument owns the history it covered, and the clamp is what keeps the slot's exclusion
    // constraint satisfied. `parameter_id` is authored here (the derive-from-sensor trigger was
    // dropped).
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"INSERT INTO sensor_deployments
                  (id, sensor_id, site_id, parameter_id, deployed_from, deployment_type, notes)
              SELECT gen_random_uuid(), $1, $2, $3,
                     GREATEST($4::timestamptz, COALESCE((
                         SELECT MAX(d.deployed_until) FROM sensor_deployments d
                         WHERE d.site_id = $2
                           AND d.parameter_id = $3
                           AND d.deployed_until IS NOT NULL
                     ), $4::timestamptz)),
                     'permanent', 'Auto-created during stream pairing'
              WHERE NOT EXISTS (
                  SELECT 1 FROM sensor_deployments d
                  WHERE d.site_id = $2
                    AND d.parameter_id = $3
                    AND d.deployed_until IS NULL
              )
              RETURNING id",
            [
                sensor_id.into(),
                site_id.into(),
                parameter_id.into(),
                opens_at.into(),
            ],
        ))
        .await?;

    match row {
        Some(r) => Ok(Some(r.try_get("", "id")?)),
        None => {
            tracing::info!(
                %sensor_id, %site_id,
                "Deployment slot already occupied by another sensor; skipping auto-deploy (explicit adopt required)"
            );
            Ok(None)
        }
    }
}

/// Close the active deployment for a sensor at one (site, parameter) slot.
///
/// Scoped to the parameter because a multi-channel instrument holds one open deployment per
/// parameter at a site (`find_or_create_deployment`), and every other lifecycle path recalls by
/// parameter. Closing by (sensor, site) alone would end channels nothing asked about.
pub async fn close_sensor_deployment(
    db: &DatabaseConnection,
    sensor_id: Uuid,
    site_id: Uuid,
    parameter_id: Uuid,
) -> AppResult<()> {
    deployments::Entity::update_many()
        .col_expr(
            deployments::Column::DeployedUntil,
            Expr::value(Some(Utc::now())),
        )
        .filter(deployments::Column::SensorId.eq(sensor_id))
        .filter(deployments::Column::SiteId.eq(site_id))
        .filter(deployments::Column::ParameterId.eq(parameter_id))
        .filter(deployments::Column::DeployedUntil.is_null())
        .exec(db)
        .await?;
    Ok(())
}

/// Resolve attribution for a batch of reading times for one sensor, by window, the same half-open
/// `[from, COALESCE(until,'infinity'))` semantics `reprocess_sensor_readings` uses, so every write
/// path agrees with reprocess. Two indexed range scans regardless of batch size.
///
/// `expected_site`: when `Some`, only a deployment at that site can attribute a time (used by grabs,
/// which are site-fixed by the request); when `None`, whichever deployment covers the time wins
/// (matches reprocess, used by continuous ingest).
///
/// `parameter_id`: the reading's parameter. A multi-channel instrument holds one open deployment
/// per parameter at a site, so a deployment on another parameter never attributes the time. `None`
/// (an unattributed reading) matches any deployment, the same three-way predicate the reprocess
/// UPDATEs use.
pub async fn resolve_windows_for_times<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
    expected_site: Option<Uuid>,
    parameter_id: Option<Uuid>,
    times: &[chrono::DateTime<Utc>],
) -> AppResult<std::collections::HashMap<chrono::DateTime<Utc>, ResolvedSlot>> {
    use std::collections::HashMap;
    let mut out: HashMap<chrono::DateTime<Utc>, ResolvedSlot> = HashMap::new();
    if times.is_empty() {
        return Ok(out);
    }

    // Read the raw nullable bounds (NULL = open / unbounded). Do NOT COALESCE to 'infinity' in SQL
    // and read it into a non-nullable DateTime, chrono/sqlx cannot represent infinity and would
    // panic for the (common) open calibration/deployment.
    let cals: Vec<(Uuid, chrono::DateTime<Utc>, Option<chrono::DateTime<Utc>>)> =
        sensor_calibrations::Entity::find()
            .filter(sensor_calibrations::Column::SensorId.eq(sensor_id))
            .select_only()
            .column(sensor_calibrations::Column::Id)
            .column(sensor_calibrations::Column::ValidFrom)
            .column(sensor_calibrations::Column::ValidUntil)
            .order_by_asc(sensor_calibrations::Column::ValidFrom)
            .into_tuple()
            .all(db)
            .await?;

    let mut dep_query =
        deployments::Entity::find().filter(deployments::Column::SensorId.eq(sensor_id));
    if let Some(parameter_id) = parameter_id {
        dep_query = dep_query.filter(deployments::Column::ParameterId.eq(parameter_id));
    }
    let deps: Vec<(
        Uuid,
        Uuid,
        chrono::DateTime<Utc>,
        Option<chrono::DateTime<Utc>>,
    )> = dep_query
        .select_only()
        .column(deployments::Column::Id)
        .column(deployments::Column::SiteId)
        .column(deployments::Column::DeployedFrom)
        .column(deployments::Column::DeployedUntil)
        .order_by_asc(deployments::Column::DeployedFrom)
        .into_tuple()
        .all(db)
        .await?;

    for &t in times {
        // A NULL upper bound is open-ended (covers everything from `from` onward).
        let calibration_id = cals
            .iter()
            .find(|(_, from, until)| t >= *from && until.is_none_or(|u| t < u))
            .map(|(id, _, _)| *id);
        let dep = deps.iter().find(|(_, site_id, from, until)| {
            t >= *from && until.is_none_or(|u| t < u) && expected_site.is_none_or(|s| *site_id == s)
        });
        out.insert(
            t,
            ResolvedSlot {
                calibration_id,
                deployment_id: dep.map(|(id, _, _, _)| *id),
                site_id: dep.map(|(_, site_id, _, _)| *site_id),
            },
        );
    }
    Ok(out)
}

/// `(id, sensor_id, from, until)` for a deployment or calibration window row.
type SlotWindowRow = (
    Uuid,
    Uuid,
    chrono::DateTime<Utc>,
    Option<chrono::DateTime<Utc>>,
);

/// Reverse of [`resolve_windows_for_times`]: for a `(site, parameter)` slot, resolve which sensor,
/// and its deployment + active calibration, covers each time, by the same half-open
/// `[from, COALESCE(until,'infinity'))` windows. Used by the write paths (import/batch/ingest) to
/// attribute a reading at write time whenever a deployment already covers its time, so new data lands
/// attributed instead of NULL. Single-valued: the `excl_deployment_site_param_slot` constraint
/// guarantees at most one deployment per `(site, parameter)` at any instant. Times outside every
/// deployment window resolve to `ResolvedOwner::default()` (all `None`), they need a backdate.
pub async fn resolve_slot_owner_for_times<C: ConnectionTrait>(
    db: &C,
    site_id: Uuid,
    parameter_id: Uuid,
    times: &[chrono::DateTime<Utc>],
) -> AppResult<std::collections::HashMap<chrono::DateTime<Utc>, ResolvedOwner>> {
    use std::collections::HashMap;
    let mut out: HashMap<chrono::DateTime<Utc>, ResolvedOwner> = HashMap::new();
    if times.is_empty() {
        return Ok(out);
    }

    let deps: Vec<SlotWindowRow> = deployments::Entity::find()
        .filter(deployments::Column::SiteId.eq(site_id))
        .filter(deployments::Column::ParameterId.eq(parameter_id))
        .select_only()
        .column(deployments::Column::Id)
        .column(deployments::Column::SensorId)
        .column(deployments::Column::DeployedFrom)
        .column(deployments::Column::DeployedUntil)
        .order_by_asc(deployments::Column::DeployedFrom)
        .into_tuple()
        .all(db)
        .await?;
    if deps.is_empty() {
        for &t in times {
            out.insert(t, ResolvedOwner::default());
        }
        return Ok(out);
    }

    // Which deployment owns a time is answered here, because a deployment is what a slot is.
    let mut owner_at: HashMap<chrono::DateTime<Utc>, (Uuid, Uuid)> = HashMap::new();
    let mut times_by_sensor: HashMap<Uuid, Vec<chrono::DateTime<Utc>>> = HashMap::new();
    for &t in times {
        if let Some((dep_id, sensor_id, _, _)) = deps
            .iter()
            .find(|(_, _, from, until)| t >= *from && until.is_none_or(|u| t < u))
        {
            owner_at.insert(t, (*dep_id, *sensor_id));
            times_by_sensor.entry(*sensor_id).or_default().push(t);
        }
    }

    // Which curve covers a time is not. `resolver::resolve_for_times` is the one answer: it ranks a
    // parameter-matching curve over a parameter-less one, and the latest covering window over an
    // earlier one. Scanning this slot's curves in `valid_from` order instead would take the
    // earliest covering window and disagree with every other write path.
    let mut curve_at: HashMap<(Uuid, chrono::DateTime<Utc>), Uuid> = HashMap::new();
    for (sensor_id, sensor_times) in &times_by_sensor {
        let curves = sensor_calibrations::resolver::resolve_for_times(
            db,
            *sensor_id,
            Some(parameter_id),
            sensor_times,
        )
        .await?;
        for (t, curve) in curves {
            curve_at.insert((*sensor_id, t), curve.id);
        }
    }

    for &t in times {
        let (deployment_id, sensor_id) = match owner_at.get(&t) {
            Some((dep_id, sensor_id)) => (Some(*dep_id), Some(*sensor_id)),
            None => (None, None),
        };
        let calibration_id = sensor_id.and_then(|sid| curve_at.get(&(sid, t)).copied());
        out.insert(
            t,
            ResolvedOwner {
                sensor_id,
                deployment_id,
                calibration_id,
            },
        );
    }
    Ok(out)
}

/// Extract the Vaisala device serial from stream metadata (for discovery response).
pub fn extract_vaisala_device_serial(metadata: &serde_json::Value) -> Option<String> {
    metadata
        .get("device")
        .and_then(|d| d.get("logger_serial").or_else(|| d.get("probe_serial")))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Reconcile the device identity an instrument was minted with against what its feed now reports.
///
/// A logger's probe can be replaced without the channel changing, and the channel is the identity
/// (Q26), so the swap mints nothing: the readings either side of it are attributed to one
/// instrument and one calibration timeline. Nothing may fork the instrument automatically, because
/// an upstream metadata correction is indistinguishable from a physical change and forking would
/// silently re-attribute history. So the serials on the sensor are refreshed, being information
/// rather than identity, and the change is put in front of an operator to act on.
///
/// Returns whether anything differed.
pub async fn reconcile_source_identity<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
    stream_id: Uuid,
    stream_metadata: &serde_json::Value,
) -> AppResult<bool> {
    let Some(reported) = source_identity(stream_metadata) else {
        return Ok(false);
    };
    let Some(sensor) = super::Entity::find_by_id(sensor_id).one(db).await? else {
        return Ok(false);
    };
    let stored = sensor.metadata.clone().unwrap_or(serde_json::Value::Null);

    // Only the identity fields are compared; everything else on the sensor's metadata is the
    // operator's and is carried through untouched.
    let changed: Vec<&str> = ["source_device_serial", "source_probe_serial"]
        .into_iter()
        .filter(|key| stored.get(*key) != reported.get(*key))
        .collect();
    if changed.is_empty() {
        return Ok(false);
    }

    let mut merged = match stored.clone() {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    if let Some(obj) = reported.as_object() {
        for (k, v) in obj {
            merged.insert(k.clone(), v.clone());
        }
    }
    let mut active: super::ActiveModel = sensor.into();
    active.metadata = Set(Some(serde_json::Value::Object(merged)));
    active.update(db).await?;

    raise_source_identity_hold(db, stream_id, &changed, &stored, &reported).await?;
    Ok(true)
}

/// Put a device-identity change in the review queue, updating the standing hold rather than adding
/// one per sync cycle.
pub async fn raise_source_identity_hold<C: ConnectionTrait>(
    db: &C,
    stream_id: Uuid,
    changed: &[&str],
    stored: &serde_json::Value,
    reported: &serde_json::Value,
) -> AppResult<()> {
    // One statement, because two overlapping registrations see neither each other's UPDATE nor
    // each other's uncommitted row: `replicate_audit_holds_identity_live_uniq` is the conflict
    // target, so the second pass waits and then updates the standing hold.
    audit::upsert_hold(
        db,
        &audit::Hold {
            key: audit::HoldKey::StreamStanding { stream_id },
            kind: HoldKind::SourceIdentityChanged,
            expected: serde_json::json!({ "was": stored, "fields": changed }),
            computed: serde_json::json!({ "now": reported }),
            delta: serde_json::json!({}),
            status: HoldStatus::Pending,
            tool: None,
        },
    )
    .await
}

#[cfg(test)]
#[path = "tests/service.rs"]
mod tests;
