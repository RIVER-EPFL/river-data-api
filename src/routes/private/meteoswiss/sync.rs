use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DbErr, Statement};
use std::collections::BTreeMap;
use uuid::Uuid;

use super::parse;
use crate::config::Config;
use crate::routes::private::reprocessing_jobs::job::Job;
use crate::routes::private::reprocessing_jobs::lifecycle::{JobContext, JobReport};
use crate::routes::private::reprocessing_jobs::schedule::Schedule;

/// The catalog parameter the feed lands on, seeded by `m20260907_000007_meteoswiss_pressure`.
const PARAMETER_CODE: &str = "barometric_pressure";
/// The SMN variable: station-level pressure, in hectopascals.
const VARIABLE: &str = "prestas0";
const SOURCE_SYSTEM: &str = "meteoswiss";
/// Rows per INSERT. Two placeholders per row plus four constants stays far inside the bind limit.
const CHUNK: usize = 500;

/// A site that has declared which station reports for it.
pub struct Subscriber {
    pub site_id: Uuid,
    pub site_name: String,
    pub station: String,
}

/// Pull each declared station's recent file and land its pressure at every site that named it.
pub struct MeteoswissSync {
    base_url: String,
    interval_seconds: u64,
    timeout_seconds: u64,
}

impl MeteoswissSync {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            base_url: config.meteoswiss_base_url.clone(),
            interval_seconds: config.meteoswiss_interval_seconds,
            timeout_seconds: config.meteoswiss_timeout_seconds,
        }
    }
}

#[async_trait]
impl Job for MeteoswissSync {
    fn name(&self) -> &'static str {
        "meteoswiss_sync"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(
            i64::try_from(self.interval_seconds.max(1)).unwrap_or(3600),
        ))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let subscribers = subscribers(ctx.db()).await?;
        if subscribers.is_empty() {
            ctx.report(JobReport::new().count("stations", 0usize)).await;
            return Ok(0);
        }

        let Some(parameter_id) = parameter_id(ctx.db()).await? else {
            ctx.log(
                "warn",
                "No barometric_pressure parameter in the catalog; nothing to land on",
                serde_json::json!({ "code": PARAMETER_CODE }),
            )
            .await;
            return Ok(0);
        };

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(self.timeout_seconds.max(1)))
            .build()
            .map_err(|e| DbErr::Custom(format!("Failed to build the MeteoSwiss client: {e}")))?;

        // One fetch per station, however many sites named it.
        let mut by_station: BTreeMap<String, Vec<Subscriber>> = BTreeMap::new();
        for subscriber in subscribers {
            by_station
                .entry(subscriber.station.clone())
                .or_default()
                .push(subscriber);
        }

        let mut inserted = 0usize;
        let mut stations_read = 0usize;
        let mut stations_failed = 0usize;
        let mut blank = 0usize;
        let mut unreadable = 0usize;
        let mut earliest: Option<DateTime<Utc>> = None;

        for (station, sites) in by_station {
            if ctx.is_cancelled() {
                break;
            }
            let url = parse::recent_url(&self.base_url, &station);
            let series = match fetch(&client, &url).await.and_then(|body| {
                parse::series(&body, VARIABLE).map_err(|e| format!("{url}: {e}"))
            }) {
                Ok(series) => series,
                Err(e) => {
                    stations_failed += 1;
                    ctx.log(
                        "warn",
                        "Could not read a MeteoSwiss station",
                        serde_json::json!({ "station": station, "error": e }),
                    )
                    .await;
                    continue;
                }
            };
            stations_read += 1;
            blank += series.blank;
            unreadable += series.unreadable;

            for site in sites {
                let stream_id = provision(ctx.db(), &site, parameter_id).await?;
                let cursor = cursor(ctx.db(), stream_id).await?;
                let fresh: Vec<&parse::Point> = series
                    .points
                    .iter()
                    .filter(|p| cursor.is_none_or(|c| p.time > c))
                    .collect();
                if fresh.is_empty() {
                    continue;
                }
                let sensor_id = instrument(ctx.db(), &station).await?;
                let written =
                    insert(ctx.db(), stream_id, site.site_id, parameter_id, sensor_id, &fresh)
                        .await?;
                inserted += written;

                let newest = fresh.iter().map(|p| p.time).max();
                let oldest = fresh.iter().map(|p| p.time).min();
                earliest = match (earliest, oldest) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                if let Some(newest) = newest {
                    advance_cursor(ctx.db(), stream_id, newest).await?;
                }
            }
        }

        // A pressure series rolls up like any other continuous parameter, so the rollups have to
        // see what just landed or the site charts serve a gap.
        if let Some(since) = earliest {
            crate::common::sync_state::refresh_continuous_aggregates(ctx.db(), Some(since))
                .await
                .map_err(|e| DbErr::Custom(format!("Aggregate refresh failed: {e}")))?;
        }

        ctx.report(
            JobReport::new()
                .scope("variable", VARIABLE)
                .scope_opt("since", earliest.map(|t| t.to_rfc3339()))
                .count("stations", stations_read)
                .count("stations_failed", stations_failed)
                .count("readings_inserted", inserted)
                .count("blank_cells", blank)
                .count("unreadable_rows", unreadable),
        )
        .await;
        Ok(i64::try_from(inserted).unwrap_or(i64::MAX))
    }
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let response = client.get(url).send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("{url}: HTTP {status}"));
    }
    response.text().await.map_err(|e| e.to_string())
}

pub async fn subscribers<C: ConnectionTrait>(db: &C) -> Result<Vec<Subscriber>, DbErr> {
    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, name, upper(btrim(meteoswiss_station_abbr)) AS station
               FROM sites
              WHERE btrim(coalesce(meteoswiss_station_abbr, '')) <> ''
              ORDER BY name",
        ))
        .await?;
    rows.iter()
        .map(|row| {
            Ok(Subscriber {
                site_id: row.try_get("", "id")?,
                site_name: row.try_get("", "name")?,
                station: row.try_get("", "station")?,
            })
        })
        .collect()
}

pub async fn parameter_id<C: ConnectionTrait>(db: &C) -> Result<Option<Uuid>, DbErr> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM parameters WHERE lower(code) = $1",
            [PARAMETER_CODE.into()],
        ))
        .await?;
    row.map(|r| r.try_get::<Uuid>("", "id")).transpose()
}

/// The station as an instrument: one row per station, shared by every site that reads it.
pub async fn instrument<C: ConnectionTrait>(db: &C, station: &str) -> Result<Uuid, DbErr> {
    let existing = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM sensors WHERE source_system = $1 AND source_key = $2",
            [SOURCE_SYSTEM.into(), station.into()],
        ))
        .await?;
    if let Some(row) = existing {
        return row.try_get("", "id");
    }
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO sensors (name, manufacturer, model, source_system, source_key, kind,
                                  data_frequency, metadata)
             VALUES ($1, 'MeteoSwiss', 'SMN', $2, $3, 'device', 'high',
                     jsonb_build_object('station_abbr', $3::text, 'variable', $4::text))
             ON CONFLICT (source_system, source_key)
               WHERE source_system IS NOT NULL AND source_key IS NOT NULL
               DO UPDATE SET source_key = EXCLUDED.source_key
             RETURNING id",
            [
                format!("MeteoSwiss {station}").into(),
                SOURCE_SYSTEM.into(),
                station.into(),
                VARIABLE.into(),
            ],
        ))
        .await?
        .ok_or_else(|| DbErr::Custom("Failed to mint the MeteoSwiss station instrument".into()))?;
    row.try_get("", "id")
}

/// The site's pressure slot and the stream feeding it, created on first sync. Declaring the station
/// on the site is the whole operator action; the slot and the stream follow from it.
pub async fn provision<C: ConnectionTrait>(
    db: &C,
    site: &Subscriber,
    parameter_id: Uuid,
) -> Result<Uuid, DbErr> {
    let site_parameter_id = match db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM site_parameters WHERE site_id = $1 AND parameter_id = $2 LIMIT 1",
            [site.site_id.into(), parameter_id.into()],
        ))
        .await?
    {
        Some(row) => row.try_get::<Uuid>("", "id")?,
        None => db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "INSERT INTO site_parameters
                     (site_id, parameter_id, name, display_units, sample_interval_sec, needs_review)
                 VALUES ($1, $2, $3, 'hPa', 600, true)
                 RETURNING id",
                [
                    site.site_id.into(),
                    parameter_id.into(),
                    format!("{} Barometric Pressure", site.site_name).into(),
                ],
            ))
            .await?
            .ok_or_else(|| DbErr::Custom("Failed to create the pressure slot".into()))?
            .try_get::<Uuid>("", "id")?,
    };

    let source_key = format!("{}:{VARIABLE}:{}", site.station, site.site_id);
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO data_streams
                 (source_system, source_key, source_name, site_parameter_id, measurement_type,
                  paired_at, metadata)
             VALUES ($1, $2, $3, $4, 'continuous', NOW(),
                     jsonb_build_object('station', $5::text, 'variable', $6::text,
                                        'decimal_places', 1))
             ON CONFLICT (source_system, source_key) DO UPDATE
                SET site_parameter_id = EXCLUDED.site_parameter_id,
                    paired_at = COALESCE(data_streams.paired_at, EXCLUDED.paired_at)
             RETURNING id",
            [
                SOURCE_SYSTEM.into(),
                source_key.into(),
                format!("{} {VARIABLE}", site.station).into(),
                site_parameter_id.into(),
                site.station.clone().into(),
                VARIABLE.into(),
            ],
        ))
        .await?
        .ok_or_else(|| DbErr::Custom("Failed to register the MeteoSwiss stream".into()))?;
    row.try_get("", "id")
}

pub async fn cursor<C: ConnectionTrait>(
    db: &C,
    stream_id: Uuid,
) -> Result<Option<DateTime<Utc>>, DbErr> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT last_data_time FROM data_streams WHERE id = $1",
            [stream_id.into()],
        ))
        .await?;
    match row {
        Some(row) => row.try_get::<Option<DateTime<Utc>>>("", "last_data_time"),
        None => Ok(None),
    }
}

pub async fn advance_cursor<C: ConnectionTrait>(
    db: &C,
    stream_id: Uuid,
    newest: DateTime<Utc>,
) -> Result<(), DbErr> {
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE data_streams
            SET last_data_time = GREATEST(COALESCE(last_data_time, $2), $2), updated_at = NOW()
          WHERE id = $1",
        [stream_id.into(), newest.into()],
    ))
    .await?;
    Ok(())
}

/// The SMN file is append-only and re-publishes what it already held, so a replayed instant is a
/// duplicate rather than a correction: the conflict does nothing.
pub async fn insert<C: ConnectionTrait>(
    db: &C,
    stream_id: Uuid,
    site_id: Uuid,
    parameter_id: Uuid,
    sensor_id: Uuid,
    points: &[&parse::Point],
) -> Result<usize, DbErr> {
    let mut written = 0usize;
    for chunk in points.chunks(CHUNK) {
        let mut values = Vec::with_capacity(chunk.len());
        let mut binds: Vec<sea_orm::Value> = vec![
            stream_id.into(),
            site_id.into(),
            parameter_id.into(),
            sensor_id.into(),
        ];
        for point in chunk {
            let time_at = binds.len() + 1;
            let value_at = binds.len() + 2;
            values.push(format!(
                "($1, ${time_at}, 0, $2, $3, ${value_at}, $4, 'continuous')"
            ));
            binds.push(point.time.into());
            binds.push(point.value.into());
        }
        let sql = format!(
            "INSERT INTO readings
                 (stream_id, \"time\", replicate_index, site_id, parameter_id, raw_value,
                  sensor_id, measurement_type)
             VALUES {}
             ON CONFLICT DO NOTHING",
            values.join(", ")
        );
        let result = db
            .execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                &sql,
                binds,
            ))
            .await?;
        written += usize::try_from(result.rows_affected()).unwrap_or(0);
    }
    Ok(written)
}
