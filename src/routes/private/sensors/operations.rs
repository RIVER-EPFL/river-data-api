use chrono::{DateTime, Utc};
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement, TransactionTrait};
use std::collections::HashMap;
use uuid::Uuid;

use super::model::Sensor;

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
                crate::routes::private::data_streams::replicates::family_keys_for_sensors(
                    db,
                    &[id],
                )
                .await
                .map_err(|e| crudcrate::ApiError::internal(e.to_string(), None))?;
            crate::routes::private::data_streams::replicates::refuse_family_retag(
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
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT \
                   EXISTS(SELECT 1 FROM readings WHERE sensor_id = $1) AS readings, \
                   EXISTS(SELECT 1 FROM standard_curves WHERE sensor_id = $1) AS curves, \
                   EXISTS(SELECT 1 FROM sensor_calibrations WHERE sensor_id = $1) AS calibrations, \
                   EXISTS(SELECT 1 FROM sensor_deployments WHERE sensor_id = $1) AS deployments",
                [id.into()],
            ))
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
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sensor_id, COUNT(*) AS n FROM readings \
                  WHERE sensor_id = ANY($1) AND time > now() - INTERVAL '90 days' \
                    AND measurement_type = 'spot' AND is_flagged IS NOT TRUE \
                  GROUP BY sensor_id",
            values.clone(),
        ))
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
            .and_modify(|cur| *cur = (*cur).max(t))
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
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT DISTINCT ON (sensor_id) sensor_id, time, COALESCE(calibrated_value, raw_value) AS value
                  FROM readings
                  WHERE sensor_id = ANY($1) AND time > now() - INTERVAL '90 days'
                  ORDER BY sensor_id, time DESC",
                [uncovered.into()],
            ))
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
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT DISTINCT ON (sensor_id) sensor_id, time, COALESCE(calibrated_value, raw_value) AS value
                  FROM readings
                  WHERE sensor_id = ANY($1) AND time >= $2 AND time <= $3
                  ORDER BY sensor_id, time DESC",
                [
                    cluster.into(),
                    sea_orm::prelude::DateTimeWithTimeZone::from(lo - chrono::Duration::days(1))
                        .into(),
                    sea_orm::prelude::DateTimeWithTimeZone::from(hi).into(),
                ],
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
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT sc.sensor_id, COUNT(DISTINCT sc.id) AS curves, MAX(r.time) AS last_use
                  FROM standard_curves sc
                  LEFT JOIN readings r ON r.standard_curve_id = sc.id
                  WHERE sc.sensor_id = ANY($1)
                  GROUP BY sc.sensor_id",
            [ids.to_vec().into()],
        ))
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
