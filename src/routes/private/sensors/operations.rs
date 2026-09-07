use async_trait::async_trait;
use chrono::{DateTime, Utc};
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, EntityTrait,
    QueryFilter, Set, Statement,
};
use std::collections::HashMap;
use uuid::Uuid;

use super::model::Sensor;
use crate::routes::private::sync::replicate_audit as audit;
use crate::error::{AppError, AppResult};
use crate::routes::private::{data_streams, sensors, sensors::deployments};

pub struct SensorOperations;

fn build_in_clause(count: usize) -> String {
    (1..=count)
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn uuid_values(ids: &[Uuid]) -> Vec<sea_orm::Value> {
    ids.iter().map(|id| (*id).into()).collect()
}

#[async_trait]
impl CRUDOperations for SensorOperations {
    type Resource = Sensor;

    /// Mirrors `/sensors/retag_frequency`: a sensor reaching a replicate-family stream cannot be
    /// classified high-frequency through entity CRUD either, so the two routes to the same column
    /// hold the same rule.
    async fn before_update(
        &self,
        db: &DatabaseConnection,
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
                "continuous",
            )
            .map_err(|e| crudcrate::ApiError::bad_request(e.to_string()))?;
        }
        Ok(())
    }

    /// One row at a time, through the single-row path: the crudcrate default `create_many`
    /// delegates to the resource, which delegates back here, so the default recurses; the loop
    /// also runs the single-row hooks for every item.
    async fn create_many(
        &self,
        db: &DatabaseConnection,
        data: Vec<<Sensor as CRUDResource>::CreateModel>,
    ) -> Result<Vec<Sensor>, ApiError> {
        let mut created = Vec::with_capacity(data.len());
        for item in data {
            created.push(self.create(db, item).await?);
        }
        Ok(created)
    }

    /// The dependent tables all reference sensors without ON DELETE, so the constraint would
    /// refuse this anyway; the check turns that into a stated 400 instead of an internal error.
    async fn before_delete(&self, db: &DatabaseConnection, id: Uuid) -> Result<(), ApiError> {
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
            for (col, label) in [
                ("readings", "readings"),
                ("curves", "standard curves"),
                ("calibrations", "calibrations"),
                ("deployments", "deployments"),
            ] {
                if row.try_get::<bool>("", col).unwrap_or(false) {
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

    /// One row at a time, through the single-row path, so a bulk edit meets the same guard.
    async fn update_many(
        &self,
        db: &DatabaseConnection,
        updates: Vec<(Uuid, <Sensor as CRUDResource>::UpdateModel)>,
    ) -> Result<Vec<Sensor>, crudcrate::ApiError> {
        let mut updated = Vec::with_capacity(updates.len());
        for (id, data) in updates {
            updated.push(self.update(db, id, data).await?);
        }
        Ok(updated)
    }

    async fn before_delete_many(
        &self,
        db: &DatabaseConnection,
        ids: &[Uuid],
    ) -> Result<(), ApiError> {
        for id in ids {
            self.before_delete(db, *id).await?;
        }
        Ok(())
    }

    async fn after_get_one(
        &self,
        db: &DatabaseConnection,
        entity: &mut Sensor,
    ) -> Result<(), ApiError> {
        let mut enriched = enrich(db, &[entity.id]).await?;
        if let Some(fields) = enriched.remove(&entity.id) {
            fields.apply(
                &mut entity.current_site_id,
                &mut entity.current_site_name,
                &mut entity.last_calibration_at,
                &mut entity.last_reading_at,
                &mut entity.last_reading_value,
                &mut entity.reading_count,
            );
        }
        Ok(())
    }

    async fn after_get_all(
        &self,
        db: &DatabaseConnection,
        entities: &mut Vec<<Sensor as CRUDResource>::ListModel>,
    ) -> Result<(), ApiError> {
        if entities.is_empty() {
            return Ok(());
        }
        let ids: Vec<Uuid> = entities.iter().map(|e| e.id).collect();
        let enriched = enrich(db, &ids).await?;
        for entity in entities.iter_mut() {
            if let Some(fields) = enriched.get(&entity.id) {
                fields.clone().apply(
                    &mut entity.current_site_id,
                    &mut entity.current_site_name,
                    &mut entity.last_calibration_at,
                    &mut entity.last_reading_at,
                    &mut entity.last_reading_value,
                    &mut entity.reading_count,
                );
            }
        }
        Ok(())
    }
}

/// What both sensor read paths report over the stored row: where the instrument is, when it was
/// last calibrated, and what it last measured. One shape, so the detail and the list cannot
/// disagree about a sensor.
#[derive(Clone, Debug, Default)]
struct Enrichment {
    current_site: Option<(Uuid, String)>,
    last_calibration_at: Option<DateTime<Utc>>,
    last_reading_at: Option<DateTime<Utc>>,
    last_reading_value: Option<f64>,
    reading_count: Option<i64>,
}

impl Enrichment {
    /// The detail and list models are separate types carrying the same six fields, so they are
    /// written through by reference rather than by a trait neither of them implements.
    fn apply(
        self,
        current_site_id: &mut Option<Uuid>,
        current_site_name: &mut Option<String>,
        last_calibration_at: &mut Option<DateTime<Utc>>,
        last_reading_at: &mut Option<DateTime<Utc>>,
        last_reading_value: &mut Option<f64>,
        reading_count: &mut Option<i64>,
    ) {
        if let Some((site_id, site_name)) = self.current_site {
            *current_site_id = Some(site_id);
            *current_site_name = Some(site_name);
        }
        if self.last_calibration_at.is_some() {
            *last_calibration_at = self.last_calibration_at;
        }
        if self.last_reading_at.is_some() {
            *last_reading_at = self.last_reading_at;
        }
        if self.last_reading_value.is_some() {
            *last_reading_value = self.last_reading_value;
        }
        if self.reading_count.is_some() {
            *reading_count = self.reading_count;
        }
    }
}

/// Resolve [`Enrichment`] for a set of sensors in a fixed number of queries, whatever the set's
/// size.
///
/// Summaries instead of an unbounded readings scan, whose planning cost grows with the
/// hypertable's chunk count: the count is the hourly rollup's population plus recent spot rows,
/// and the newest instant is the stream ingest cursor with the rollup's newest bucket as fallback.
async fn enrich(
    db: &DatabaseConnection,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Enrichment>, ApiError> {
    let mut out: HashMap<Uuid, Enrichment> = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let placeholders = build_in_clause(ids.len());
    let values = uuid_values(ids);

    // Where the instrument is now.
    let dep_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                r"SELECT sd.sensor_id, sd.site_id, s.name AS site_name
                  FROM sensor_deployments sd JOIN sites s ON s.id = sd.site_id
                  WHERE sd.sensor_id IN ({placeholders}) AND sd.deployed_until IS NULL"
            ),
            values.clone(),
        ))
        .await
        .map_err(ApiError::database)?;
    for row in &dep_rows {
        if let (Ok(sensor_id), Ok(site_id), Ok(site_name)) = (
            row.try_get::<Uuid>("", "sensor_id"),
            row.try_get::<Uuid>("", "site_id"),
            row.try_get::<String>("", "site_name"),
        ) {
            let entry = out.entry(sensor_id).or_default();
            if entry.current_site.is_none() {
                entry.current_site = Some((site_id, site_name));
            }
        }
    }

    // When it was last calibrated.
    let cal_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                r"SELECT DISTINCT ON (sensor_id) sensor_id, valid_from
                  FROM sensor_calibrations
                  WHERE sensor_id IN ({placeholders})
                  ORDER BY sensor_id, valid_from DESC"
            ),
            values.clone(),
        ))
        .await
        .map_err(ApiError::database)?;
    for row in &cal_rows {
        if let (Ok(sensor_id), Ok(valid_from)) = (
            row.try_get::<Uuid>("", "sensor_id"),
            row.try_get::<DateTime<chrono::FixedOffset>>("", "valid_from"),
        ) {
            out.entry(sensor_id).or_default().last_calibration_at =
                Some(valid_from.with_timezone(&Utc));
        }
    }

    // How much it has measured: the rollup's population plus the spot rows it does not carry.
    let count_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT sensor_id, COALESCE(SUM(count), 0)::bigint AS n FROM readings_hourly \
                  WHERE sensor_id IN ({placeholders}) GROUP BY sensor_id"
            ),
            values.clone(),
        ))
        .await
        .map_err(ApiError::database)?;
    for row in &count_rows {
        if let (Ok(sensor_id), Ok(n)) = (
            row.try_get::<Uuid>("", "sensor_id"),
            row.try_get::<i64>("", "n"),
        ) {
            let entry = out.entry(sensor_id).or_default();
            entry.reading_count = Some(entry.reading_count.unwrap_or(0) + n);
        }
    }
    let spot_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT sensor_id, COUNT(*) AS n FROM readings \
                  WHERE sensor_id IN ({placeholders}) AND time > now() - INTERVAL '90 days' \
                    AND measurement_type = 'spot' AND is_flagged IS NOT TRUE \
                  GROUP BY sensor_id"
            ),
            values.clone(),
        ))
        .await
        .map_err(ApiError::database)?;
    for row in &spot_rows {
        if let (Ok(sensor_id), Ok(n)) = (
            row.try_get::<Uuid>("", "sensor_id"),
            row.try_get::<i64>("", "n"),
        ) {
            let entry = out.entry(sensor_id).or_default();
            entry.reading_count = Some(entry.reading_count.unwrap_or(0) + n);
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
            format!(
                r"SELECT sensor_id, MAX(last_data_time) AS last_time
                  FROM data_streams
                  WHERE sensor_id IN ({placeholders}) AND last_data_time IS NOT NULL
                  GROUP BY sensor_id"
            ),
            values.clone(),
        ))
        .await
        .map_err(ApiError::database)?;
    let bucket_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT sensor_id, MAX(bucket) AS last_bucket FROM readings_hourly \
                  WHERE sensor_id IN ({placeholders}) GROUP BY sensor_id"
            ),
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
        if let (Ok(sensor_id), Ok(t)) = (
            row.try_get::<Uuid>("", "sensor_id"),
            row.try_get::<DateTime<chrono::FixedOffset>>("", "last_time"),
        ) {
            note(&mut newest, sensor_id, t.with_timezone(&Utc));
            note(&mut window_end, sensor_id, t.with_timezone(&Utc));
        }
    }
    for row in &bucket_rows {
        if let (Ok(sensor_id), Ok(t)) = (
            row.try_get::<Uuid>("", "sensor_id"),
            row.try_get::<DateTime<chrono::FixedOffset>>("", "last_bucket"),
        ) {
            let bucket = t.with_timezone(&Utc);
            note(&mut newest, sensor_id, bucket);
            note(
                &mut window_end,
                sensor_id,
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
        let ph = build_in_clause(uncovered.len());
        let rows = db
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    r"SELECT DISTINCT ON (sensor_id) sensor_id, time, COALESCE(calibrated_value, raw_value) AS value
                      FROM readings
                      WHERE sensor_id IN ({ph}) AND time > now() - INTERVAL '90 days'
                      ORDER BY sensor_id, time DESC"
                ),
                uuid_values(&uncovered),
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
        let cluster_ph = build_in_clause(cluster.len());
        let mut cluster_values = uuid_values(&cluster);
        cluster_values.push(
            sea_orm::prelude::DateTimeWithTimeZone::from(lo - chrono::Duration::days(1)).into(),
        );
        let from_ref = cluster_values.len();
        cluster_values.push(sea_orm::prelude::DateTimeWithTimeZone::from(hi).into());
        let to_ref = cluster_values.len();
        let rows = db
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    r"SELECT DISTINCT ON (sensor_id) sensor_id, time, COALESCE(calibrated_value, raw_value) AS value
                      FROM readings
                      WHERE sensor_id IN ({cluster_ph}) AND time >= ${from_ref} AND time <= ${to_ref}
                      ORDER BY sensor_id, time DESC"
                ),
                cluster_values,
            ))
            .await
            .map_err(ApiError::database)?;
        record_values(&rows, &mut out);
    }

    Ok(out)
}

/// The instant and value of each probed row, over whatever the enrichment already holds.
fn record_values(rows: &[sea_orm::QueryResult], out: &mut HashMap<Uuid, Enrichment>) {
    for row in rows {
        if let (Ok(sensor_id), Ok(time), Ok(value)) = (
            row.try_get::<Uuid>("", "sensor_id"),
            row.try_get::<DateTime<chrono::FixedOffset>>("", "time"),
            row.try_get::<f64>("", "value"),
        ) {
            let entry = out.entry(sensor_id).or_default();
            entry.last_reading_at = Some(time.with_timezone(&Utc));
            entry.last_reading_value = Some(value);
        }
    }
}

/// Resolved sensor context for readings.
#[derive(Debug, Clone)]
pub struct SensorContext {
    pub sensor_id: Uuid,
    /// `None` when the sensor is not deployed to the target site at this time, the slot may be
    /// occupied by another sensor, or the sensor isn't adopted yet. Readings still carry
    /// `sensor_id`; the deployment FK is absent.
    pub deployment_id: Option<Uuid>,
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

/// What an instrument row is. Three of the four minting paths produce something that is not a
/// device, and a picker offering all four asks an operator to tell a spectrophotometer from a
/// bookkeeping row with nothing to go on. It is a display and selection attribute:
/// `(source_system, source_key)` stays the identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstrumentKind {
    /// A physical instrument: a probe, a logger channel, a lab device with a serial.
    Device,
    /// A portal curve label, one row per analyte.
    Lab,
    /// A source's instrument for one parameter, across every station it reports.
    SourceParameter,
    /// A slot's own hand-entry channel (grab entry, API batch).
    EntryChannel,
}

impl InstrumentKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Device => "device",
            Self::Lab => "lab",
            Self::SourceParameter => "source_parameter",
            Self::EntryChannel => "entry_channel",
        }
    }

    /// Everything that is not a field device has sat under this flag since before the kinds were
    /// distinguished, and the inventory's Lab tab still reads it.
    #[must_use]
    pub fn is_lab_instrument(self) -> bool {
        self != Self::Device
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
    let is_lab_instrument = kind.is_lab_instrument();
    let metadata_val: sea_orm::Value = match &metadata {
        Some(v) => serde_json::to_string(v)
            .unwrap_or_else(|_| "null".to_string())
            .into(),
        None => sea_orm::Value::String(None),
    };

    let inserted = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r#"INSERT INTO sensors
                   (id, name, is_active, is_lab_instrument, data_frequency,
                    source_system, source_key, metadata, kind, created_at)
               VALUES (gen_random_uuid(), $1, true, $2, $3, $4, $5, $6::jsonb, $7, now())
               ON CONFLICT (source_system, source_key)
                   WHERE source_system IS NOT NULL AND source_key IS NOT NULL
               DO NOTHING
               RETURNING id"#,
            [
                name.into(),
                is_lab_instrument.into(),
                data_frequency.into(),
                source_system.into(),
                source_key.into(),
                metadata_val,
                kind.as_str().into(),
            ],
        ))
        .await?;

    if let Some(row) = inserted {
        let id: Uuid = row.try_get("", "id")?;
        return Ok(id);
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
    if is_device_feed(&stream.metadata) {
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
        Some(serde_json::json!({ "minted_from_stream": stream.source_key })),
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
pub async fn create_sensor_for_stream<C: ConnectionTrait>(
    db: &C,
    stream: &data_streams::Model,
    parameter_id: Uuid,
    site_id: Uuid,
) -> AppResult<SensorContext> {
    let name = slot_instrument_name(db, site_id, parameter_id).await?;
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

/// A device feed is one whose stream metadata carries a `device` block. Broader than testing for a
/// serial: viewLinc may report a channel with no `logger_serial`, and that is still a device.
#[must_use]
pub fn is_device_feed(stream_metadata: &serde_json::Value) -> bool {
    stream_metadata.get("device").is_some_and(|d| !d.is_null())
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
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT s.name AS site_name, p.name AS parameter_name \
             FROM sites s, parameters p WHERE s.id = $1 AND p.id = $2",
            [site_id.into(), parameter_id.into()],
        ))
        .await?;
    let Some(row) = row else { return Ok(None) };
    let site: String = row.try_get("", "site_name")?;
    let parameter: String = row.try_get("", "parameter_name")?;
    Ok(Some(format!("{site} {parameter}")))
}

/// Import-only: create or reuse the instrument for a stream and resolve its latest calibration,
/// WITHOUT deploying it to a site. The "imported, not adopted" state: readings get `sensor_id`
/// (and the instrument's latest curve, if it has one) but no `deployment_id`/`site_id` until an
/// explicit adopt. Idempotent: reuses the stream's linked instrument, else the one already holding
/// the stream's channel identity, else mints it (race-safe via [`upsert_source_instrument`]).
/// Updates `data_streams.sensor_id`.
///
/// `name_hint` is the slot name when the caller knows which slot the stream is being paired to;
/// the import endpoint has no slot yet and passes `None`.
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
            "high",
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
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT MIN(time) AS first_reading FROM readings WHERE stream_id = $1",
            [stream_id.into()],
        ))
        .await?;
    let first = row
        .and_then(|r| r.try_get::<Option<DateTime<Utc>>>("", "first_reading").ok())
        .flatten();
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
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE sensor_deployments
          SET deployed_until = $1
          WHERE sensor_id = $2 AND site_id = $3 AND parameter_id = $4 AND deployed_until IS NULL",
        [
            Utc::now().into(),
            sensor_id.into(),
            site_id.into(),
            parameter_id.into(),
        ],
    ))
    .await?;
    Ok(())
}

/// One resolved attribution slot for a reading time.
#[derive(Debug, Clone, Default)]
pub struct ResolvedSlot {
    pub calibration_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
    pub site_id: Option<Uuid>,
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
    let cal_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id, valid_from, valid_until
              FROM sensor_calibrations WHERE sensor_id = $1 ORDER BY valid_from",
            [sensor_id.into()],
        ))
        .await?;
    let cals: Vec<(Uuid, chrono::DateTime<Utc>, Option<chrono::DateTime<Utc>>)> = cal_rows
        .iter()
        .map(|r| -> AppResult<_> {
            let id: Uuid = r.try_get("", "id")?;
            let from: chrono::DateTime<chrono::FixedOffset> = r.try_get("", "valid_from")?;
            let until: Option<chrono::DateTime<chrono::FixedOffset>> =
                r.try_get("", "valid_until")?;
            Ok((
                id,
                from.with_timezone(&Utc),
                until.map(|u| u.with_timezone(&Utc)),
            ))
        })
        .collect::<AppResult<_>>()?;

    let dep_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id, site_id, deployed_from, deployed_until
              FROM sensor_deployments
              WHERE sensor_id = $1 AND ($2::uuid IS NULL OR parameter_id = $2)
              ORDER BY deployed_from",
            [sensor_id.into(), parameter_id.into()],
        ))
        .await?;
    let deps: Vec<(
        Uuid,
        Uuid,
        chrono::DateTime<Utc>,
        Option<chrono::DateTime<Utc>>,
    )> = dep_rows
        .iter()
        .map(|r| -> AppResult<_> {
            let id: Uuid = r.try_get("", "id")?;
            let site_id: Uuid = r.try_get("", "site_id")?;
            let from: chrono::DateTime<chrono::FixedOffset> = r.try_get("", "deployed_from")?;
            let until: Option<chrono::DateTime<chrono::FixedOffset>> =
                r.try_get("", "deployed_until")?;
            Ok((
                id,
                site_id,
                from.with_timezone(&Utc),
                until.map(|u| u.with_timezone(&Utc)),
            ))
        })
        .collect::<AppResult<_>>()?;

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

/// Owner (sensor + deployment + active calibration) resolved for a reading time at a slot.
#[derive(Debug, Clone, Default)]
pub struct ResolvedOwner {
    pub sensor_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
    pub calibration_id: Option<Uuid>,
}

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

    let dep_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id, sensor_id, deployed_from, deployed_until
              FROM sensor_deployments WHERE site_id = $1 AND parameter_id = $2 ORDER BY deployed_from",
            [site_id.into(), parameter_id.into()],
        ))
        .await?;
    let deps: Vec<SlotWindowRow> = dep_rows
        .iter()
        .map(|r| -> AppResult<_> {
            let id: Uuid = r.try_get("", "id")?;
            let sensor_id: Uuid = r.try_get("", "sensor_id")?;
            let from: chrono::DateTime<chrono::FixedOffset> = r.try_get("", "deployed_from")?;
            let until: Option<chrono::DateTime<chrono::FixedOffset>> =
                r.try_get("", "deployed_until")?;
            Ok((
                id,
                sensor_id,
                from.with_timezone(&Utc),
                until.map(|u| u.with_timezone(&Utc)),
            ))
        })
        .collect::<AppResult<_>>()?;
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
        let curves = sensors::calibrations::resolver::resolve_for_times(
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
            key: audit::HoldKey::StreamStanding {
                stream_id,
            },
            kind: "source_identity_changed",
            expected: serde_json::json!({ "was": stored, "fields": changed }),
            computed: serde_json::json!({ "now": reported }),
            delta: serde_json::json!({}),
            status: "pending",
            tool: None,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{InstrumentKind, source_instrument_name};

    #[test]
    fn test_source_instrument_name_source_parameter_carries_no_site() {
        let name = source_instrument_name(
            InstrumentKind::SourceParameter,
            "DOC_avg_ppb",
            "cnet",
            Some("FP1 DOC_avg_ppb"),
        );
        assert_eq!(name, "DOC_avg_ppb (cnet)");
    }

    #[test]
    fn test_source_instrument_name_lab_carries_no_site() {
        let name = source_instrument_name(InstrumentKind::Lab, "DOC", "cnet", Some("FP1 DOC"));
        assert_eq!(name, "DOC (cnet)");
    }

    #[test]
    fn test_source_instrument_name_entry_channel_keeps_slot_name() {
        let name = source_instrument_name(
            InstrumentKind::EntryChannel,
            "Depth",
            "grab_sample",
            Some("Martigny Depth (grab_sample)"),
        );
        assert_eq!(name, "Martigny Depth (grab_sample)");
    }

    #[test]
    fn test_source_instrument_name_falls_back_without_a_hint() {
        let name = source_instrument_name(InstrumentKind::EntryChannel, "Depth", "api", None);
        assert_eq!(name, "Depth (api)");
    }
}
