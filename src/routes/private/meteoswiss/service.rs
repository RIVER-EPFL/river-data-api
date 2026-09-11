//! Reading the SMN feed and landing it: the CSV parser, and every query the sync makes.
//!
//! The published files are semicolon-separated with a header row naming every variable the station
//! reports, one row per ten-minute interval. `reference_timestamp` is `DD.MM.YYYY HH:MM` in UTC,
//! and a variable the station did not report at that interval is an empty cell.

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, FromQueryResult,
    QueryFilter, Set, Statement,
};
use uuid::Uuid;

use super::models::{Point, Series, Subscriber};
use crate::routes::private::{
    data_streams, parameters, readings, sensors, site_parameters as site_parameters,
};

/// The catalog parameter the feed lands on, seeded by `m20260907_000007_meteoswiss_pressure`.
pub(super) const PARAMETER_CODE: &str = "barometric_pressure";
/// The SMN variable: station-level pressure, in hectopascals.
pub(super) const VARIABLE: &str = "prestas0";
const SOURCE_SYSTEM: &str = "meteoswiss";
/// Rows per INSERT. Two placeholders per row plus four constants stays far inside the bind limit.
const CHUNK: usize = 500;

const TIMESTAMP_COLUMN: &str = "reference_timestamp";
const TIMESTAMP_FORMAT: &str = "%d.%m.%Y %H:%M";

/// Read one variable out of an SMN CSV. Errors only when the file cannot name the columns asked
/// for, which is a changed publication format rather than a gap in the data.
pub fn series(csv: &str, variable: &str) -> Result<Series, String> {
    let mut lines = csv.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next().ok_or("file is empty")?;
    let header = header.strip_prefix('\u{feff}').unwrap_or(header);

    let columns: Vec<&str> = header.split(';').map(str::trim).collect();
    let time_at = column(&columns, TIMESTAMP_COLUMN)?;
    let value_at = column(&columns, variable)?;

    let mut series = Series::default();
    for line in lines {
        let cells: Vec<&str> = line.split(';').map(str::trim).collect();
        let (Some(raw_time), Some(raw_value)) = (cells.get(time_at), cells.get(value_at)) else {
            series.unreadable += 1;
            continue;
        };
        if raw_value.is_empty() {
            series.blank += 1;
            continue;
        }
        let (Ok(naive), Ok(value)) = (
            NaiveDateTime::parse_from_str(raw_time, TIMESTAMP_FORMAT),
            raw_value.parse::<f64>(),
        ) else {
            series.unreadable += 1;
            continue;
        };
        if !value.is_finite() {
            series.unreadable += 1;
            continue;
        }
        series.points.push(Point {
            time: Utc.from_utc_datetime(&naive),
            value,
        });
    }
    Ok(series)
}

fn column(columns: &[&str], name: &str) -> Result<usize, String> {
    columns
        .iter()
        .position(|c| c.eq_ignore_ascii_case(name))
        .ok_or_else(|| format!("column {name:?} is not in the header"))
}

/// The published path for one station's recent file, under the OGD collection base URL.
#[must_use]
pub fn recent_url(base: &str, station_abbr: &str) -> String {
    let station = station_abbr.trim().to_lowercase();
    format!(
        "{}/{station}/ogd-smn_{station}_t_recent.csv",
        base.trim_end_matches('/')
    )
}

pub async fn subscribers<C: ConnectionTrait>(db: &C) -> Result<Vec<Subscriber>, DbErr> {
    Subscriber::find_by_statement(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT id AS site_id, name AS site_name,
                upper(btrim(meteoswiss_station_abbr)) AS station
               FROM sites
              WHERE btrim(coalesce(meteoswiss_station_abbr, '')) <> ''
              ORDER BY name",
    ))
    .all(db)
    .await
}

/// `lower(code) = $1`, the shape of the catalog's unique index on the code.
fn code_matches(code: &str) -> sea_orm::sea_query::SimpleExpr {
    use sea_orm::sea_query::{Expr, ExprTrait, Func};
    Expr::expr(Func::lower(Expr::col(parameters::Column::Code))).eq(code)
}

pub async fn parameter_id<C: ConnectionTrait>(db: &C) -> Result<Option<Uuid>, DbErr> {
    Ok(parameters::Entity::find()
        .filter(code_matches(PARAMETER_CODE))
        .one(db)
        .await?
        .map(|p| p.id))
}

/// The station as an instrument: one row per station, shared by every site that reads it.
pub async fn instrument<C: ConnectionTrait>(db: &C, station: &str) -> Result<Uuid, DbErr> {
    let existing = sensors::Entity::find()
        .filter(sensors::Column::SourceSystem.eq(SOURCE_SYSTEM))
        .filter(sensors::Column::SourceKey.eq(station))
        .one(db)
        .await?;
    if let Some(sensor) = existing {
        return Ok(sensor.id);
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
    let existing = site_parameters::Entity::find()
        .filter(site_parameters::Column::SiteId.eq(site.site_id))
        .filter(site_parameters::Column::ParameterId.eq(parameter_id))
        .one(db)
        .await?;
    let site_parameter_id = match existing {
        Some(slot) => slot.id,
        None => {
            site_parameters::ActiveModel {
                id: Set(Uuid::new_v4()),
                site_id: Set(site.site_id),
                parameter_id: Set(parameter_id),
                name: Set(format!("{} Barometric Pressure", site.site_name)),
                display_units: Set(Some("hPa".to_string())),
                sample_interval_sec: Set(Some(600)),
                needs_review: Set(true),
                ..Default::default()
            }
            .insert(db)
            .await?
            .id
        }
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
    Ok(data_streams::Entity::find_by_id(stream_id)
        .one(db)
        .await?
        .and_then(|stream| stream.last_data_time)
        .map(|t| t.with_timezone(&Utc)))
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
    points: &[&Point],
) -> Result<usize, DbErr> {
    let mut written = 0usize;
    for chunk in points.chunks(CHUNK) {
        let affected = insert_chunk(stream_id, site_id, parameter_id, sensor_id, chunk)
            .exec_without_returning(db)
            .await?;
        written += usize::try_from(affected).unwrap_or(0);
    }
    Ok(written)
}

/// One chunk's insert. Every column the feed knows is set and the rest take their database
/// defaults, which is what the hand-written column list did.
fn insert_chunk(
    stream_id: Uuid,
    site_id: Uuid,
    parameter_id: Uuid,
    sensor_id: Uuid,
    points: &[&Point],
) -> sea_orm::InsertMany<readings::ActiveModel> {
    readings::Entity::insert_many(points.iter().map(|point| readings::ActiveModel {
        stream_id: Set(stream_id),
        time: Set(point.time.into()),
        replicate_index: Set(0),
        site_id: Set(Some(site_id)),
        parameter_id: Set(Some(parameter_id)),
        raw_value: Set(point.value),
        sensor_id: Set(Some(sensor_id)),
        measurement_type: Set(Some("continuous".to_string())),
        ..Default::default()
    }))
    .on_conflict(OnConflict::new().do_nothing().to_owned())
}

#[cfg(test)]
#[path = "tests/service.rs"]
mod tests;
