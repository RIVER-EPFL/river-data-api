//! Reading the SMN feed and landing it: the conditional fetch, the CSV parser, and every query the
//! sync makes.
//!
//! The published files are semicolon-separated with a header row naming every variable the station
//! reports, one row per ten-minute interval. `reference_timestamp` is `DD.MM.YYYY HH:MM` in UTC,
//! and a variable the station did not report at that interval is an empty cell.

use std::collections::HashMap;

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeZone, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, FromQueryResult,
    QueryFilter, QuerySelect, Set, Statement,
};
use uuid::Uuid;

use super::models::{
    ExternalSource, Fetched, Point, Series, StationCandidate, StationRow, Subscriber, station,
    subscription,
};
use crate::routes::private::reprocessing_jobs::service as jobs;
use crate::routes::private::{data_streams, parameters, readings, sensors, site_parameters};

const SOURCE_SYSTEM: &str = "meteoswiss";

/// One SMN variable a site may subscribe to, and the catalog row it lands on. A variable the feed
/// publishes is a measurement of something the catalog has to be able to name, so the declaration
/// carries what minting that row needs.
pub struct Variable {
    /// The column in the published file.
    pub name: &'static str,
    pub code: &'static str,
    pub label: &'static str,
    pub units: &'static str,
    pub decimals: i16,
    /// What marks a station as publishing this variable, in the station list.
    pub published_by: StationMark,
}

/// The station-list column that is blank for a station publishing no such measurement. A station
/// the mark leaves out is read for ever at blank cells, so a subscription to one is refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StationMark {
    /// `height_barometer_masl`, blank for the 19 published stations carrying no barometer.
    Barometer,
}

/// Whether the station publishes the variable, by the mark the declaration names.
#[must_use]
pub fn publishes(declared: &Variable, station: &station::Model) -> bool {
    match declared.published_by {
        StationMark::Barometer => station.height_barometer_masl.is_some(),
    }
}

/// What a site may subscribe to. Station-level pressure is the variable the oxygen-saturation
/// procedure needs; another is added here with the catalog row it mints.
pub const VARIABLES: &[Variable] = &[Variable {
    name: "prestas0",
    code: "barometric_pressure",
    label: "Barometric Pressure",
    units: "hPa",
    decimals: 1,
    published_by: StationMark::Barometer,
}];

/// The declaration for a variable, by the name a subscription holds.
#[must_use]
pub fn variable(name: &str) -> Option<&'static Variable> {
    VARIABLES
        .iter()
        .find(|v| v.name.eq_ignore_ascii_case(name.trim()))
}
/// Rows per INSERT. Two placeholders per row plus four constants stays far inside the bind limit.
const CHUNK: usize = 500;

const TIMESTAMP_COLUMN: &str = "reference_timestamp";
const TIMESTAMP_FORMAT: &str = "%d.%m.%Y %H:%M";

/// The all-stations latest-values file names its station and instant differently from the
/// per-station archives, and writes a missing value as a dash.
const LATEST_STATION_COLUMN: &str = "Station/Location";
const LATEST_TIMESTAMP_COLUMN: &str = "Date";
const LATEST_TIMESTAMP_FORMAT: &str = "%Y%m%d%H%M";

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

/// Read one variable out of the all-stations latest-values file: the newest interval every station
/// reported, keyed by station abbreviation.
///
/// One file covers every station a site subscribes to, so a tick is one request however many
/// stations are read.
pub fn latest(csv: &str, variable: &str) -> Result<HashMap<String, Point>, String> {
    let mut lines = csv.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next().ok_or("file is empty")?;
    let header = header.strip_prefix('\u{feff}').unwrap_or(header);

    let columns: Vec<&str> = header.split(';').map(str::trim).collect();
    let station_at = column(&columns, LATEST_STATION_COLUMN)?;
    let time_at = column(&columns, LATEST_TIMESTAMP_COLUMN)?;
    let value_at = column(&columns, variable)?;

    let mut points: HashMap<String, Point> = HashMap::new();
    for line in lines {
        let cells: Vec<&str> = line.split(';').map(str::trim).collect();
        let (Some(station), Some(raw_time), Some(raw_value)) = (
            cells.get(station_at),
            cells.get(time_at),
            cells.get(value_at),
        ) else {
            continue;
        };
        // A station that did not report the variable at this interval writes a dash.
        if raw_value.is_empty() || *raw_value == "-" {
            continue;
        }
        let (Ok(naive), Ok(value)) = (
            NaiveDateTime::parse_from_str(raw_time, LATEST_TIMESTAMP_FORMAT),
            raw_value.parse::<f64>(),
        ) else {
            continue;
        };
        if !value.is_finite() {
            continue;
        }
        points.insert(
            station.to_uppercase(),
            Point {
                time: Utc.from_utc_datetime(&naive),
                value,
            },
        );
    }
    Ok(points)
}

fn column(columns: &[&str], name: &str) -> Result<usize, String> {
    columns
        .iter()
        .position(|c| c.eq_ignore_ascii_case(name))
        .ok_or_else(|| format!("column {name:?} is not in the header"))
}

/// The STAC item describing one station: what the collection publishes for it, as hrefs.
#[must_use]
pub fn stac_item_url(base: &str, station_abbr: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        station_abbr.trim().to_lowercase()
    )
}

/// The archives a station's STAC item lists at the ten-minute resolution, oldest decade first,
/// with the recent file last. Anything else the collection publishes (daily, hourly, monthly and
/// yearly aggregates, and the `_t_now` file the ten-minute pass already covers) is not history.
pub fn archive_hrefs(item: &serde_json::Value) -> Result<Vec<String>, String> {
    let assets = item
        .get("assets")
        .and_then(serde_json::Value::as_object)
        .ok_or("the STAC item lists no assets")?;
    let mut hrefs: Vec<(String, String)> = assets
        .iter()
        .filter(|(key, _)| key.contains("_t_historical_") || key.ends_with("_t_recent.csv"))
        .filter_map(|(key, asset)| {
            asset
                .get("href")
                .and_then(serde_json::Value::as_str)
                .map(|href| (key.clone(), href.to_string()))
        })
        .collect();
    // `_t_recent` sorts after every decade, so the pass reads oldest to newest.
    hrefs.sort();
    Ok(hrefs.into_iter().map(|(_, href)| href).collect())
}

/// Whether an archive is one of the decade files rather than the recent one.
#[must_use]
pub fn historical(href: &str) -> bool {
    href.contains("_t_historical_")
}

/// The decade a historical archive covers, read out of its published name
/// (`ogd-smn_mob_t_historical_2010-2019.csv`).
fn decade(href: &str) -> Option<(i32, i32)> {
    let tail = href.rsplit("_t_historical_").next()?;
    let years = tail.split('.').next()?;
    let (from, until) = years.split_once('-')?;
    Some((from.parse().ok()?, until.parse().ok()?))
}

/// The archives that can hold a point at or after `floor`: the recent file always, and a decade
/// file whose last year is the floor's year or later. A `None` floor is a site holding no data of
/// its own, which wants no history at all. A historical name the decade cannot be read out of is
/// kept, so a changed publication format costs a fetch rather than a site's history.
#[must_use]
pub fn archives_since(hrefs: Vec<String>, floor: Option<DateTime<Utc>>) -> Vec<String> {
    hrefs
        .into_iter()
        .filter(|href| {
            if !historical(href) {
                return true;
            }
            match (floor, decade(href)) {
                (None, _) => false,
                (Some(floor), Some((_, until))) => until >= floor.year(),
                (Some(_), None) => true,
            }
        })
        .collect()
}

#[derive(FromQueryResult)]
struct FloorRow {
    floor: Option<DateTime<Utc>>,
}

/// The earliest instant a site holds data of its own, the streams this feed provisions left out.
///
/// A fed parameter is only ever read beside the site's own measurements, so this is how far back a
/// backfill reads: an archive older than the site's first reading is history nothing at the site
/// will ever line a value up with. `None` is a site holding nothing yet.
pub async fn site_floor<C: ConnectionTrait>(
    db: &C,
    site_id: Uuid,
) -> Result<Option<DateTime<Utc>>, DbErr> {
    let fed = fed_streams(db, site_id).await?;
    let mut query = readings::Entity::find()
        .select_only()
        .expr_as(
            sea_orm::sea_query::Func::min(sea_orm::sea_query::Expr::col(readings::Column::Time)),
            "floor",
        )
        .filter(readings::Column::SiteId.eq(site_id));
    if !fed.is_empty() {
        query = query.filter(readings::Column::StreamId.is_not_in(fed));
    }
    Ok(query
        .into_model::<FloorRow>()
        .one(db)
        .await?
        .and_then(|row| row.floor))
}

/// The streams this feed provisions at a site, which speak for the station rather than for the
/// site.
async fn fed_streams<C: ConnectionTrait>(db: &C, site_id: Uuid) -> Result<Vec<Uuid>, DbErr> {
    let slots: Vec<Uuid> = site_parameters::Entity::find()
        .select_only()
        .column(site_parameters::Column::Id)
        .filter(site_parameters::Column::SiteId.eq(site_id))
        .into_tuple()
        .all(db)
        .await?;
    if slots.is_empty() {
        return Ok(Vec::new());
    }
    data_streams::Entity::find()
        .select_only()
        .column(data_streams::Column::Id)
        .filter(data_streams::Column::SourceSystem.eq(SOURCE_SYSTEM))
        .filter(data_streams::Column::SiteParameterId.is_in(slots))
        .into_tuple()
        .all(db)
        .await
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

/// The published path for the station metadata, the list every subscription names a station out of.
#[must_use]
pub fn stations_url(base: &str) -> String {
    format!("{}/ogd-smn_meta_stations.csv", base.trim_end_matches('/'))
}

const STATION_DATE_FORMAT: &str = "%d.%m.%Y";

/// Read the published station metadata. Errors only when the file cannot name the abbreviation and
/// the name, which is a changed publication format; a row missing either is skipped.
pub fn stations(csv: &str) -> Result<Vec<StationRow>, String> {
    let mut lines = csv.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next().ok_or("file is empty")?;
    let header = header.strip_prefix('\u{feff}').unwrap_or(header);

    let columns: Vec<&str> = header.split(';').map(str::trim).collect();
    let abbr_at = column(&columns, "station_abbr")?;
    let name_at = column(&columns, "station_name")?;
    let since_at = column(&columns, "station_data_since").ok();
    let height_at = column(&columns, "station_height_masl").ok();
    let barometer_at = column(&columns, "station_height_barometer_masl").ok();
    let lat_at = column(&columns, "station_coordinates_wgs84_lat").ok();
    let lon_at = column(&columns, "station_coordinates_wgs84_lon").ok();

    let mut rows = Vec::new();
    for line in lines {
        let cells: Vec<&str> = line.split(';').map(str::trim).collect();
        let (Some(abbr), Some(name)) = (cells.get(abbr_at), cells.get(name_at)) else {
            continue;
        };
        if abbr.is_empty() || name.is_empty() {
            continue;
        }
        rows.push(StationRow {
            abbr: abbr.to_uppercase(),
            name: (*name).to_string(),
            data_since: since_at
                .and_then(|at| cells.get(at))
                .and_then(|cell| NaiveDate::parse_from_str(cell, STATION_DATE_FORMAT).ok()),
            height_masl: number(&cells, height_at),
            height_barometer_masl: number(&cells, barometer_at),
            latitude: number(&cells, lat_at),
            longitude: number(&cells, lon_at),
        });
    }
    Ok(rows)
}

/// A finite number out of an optional column, which a station leaves blank where it has none.
fn number(cells: &[&str], at: Option<usize>) -> Option<f64> {
    at.and_then(|at| cells.get(at))
        .and_then(|cell| cell.parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

/// Rows per station INSERT. Seven placeholders a row keeps the whole published list in one pass.
const STATION_CHUNK: usize = 200;

/// Maintain the station list from what the metadata file yielded, keyed on the abbreviation.
/// MeteoSwiss move a station rather than renaming it, so a row already held is updated in place
/// and a subscription naming it keeps pointing at the same station.
pub async fn store_stations<C: ConnectionTrait>(
    db: &C,
    rows: &[StationRow],
) -> Result<usize, DbErr> {
    use super::models::station;
    let mut written = 0usize;
    for chunk in rows.chunks(STATION_CHUNK) {
        let affected = station::Entity::insert_many(chunk.iter().map(|row| station::ActiveModel {
            station_abbr: Set(row.abbr.clone()),
            name: Set(row.name.clone()),
            data_since: Set(row.data_since),
            height_masl: Set(row.height_masl),
            height_barometer_masl: Set(row.height_barometer_masl),
            latitude: Set(row.latitude),
            longitude: Set(row.longitude),
            updated_at: Set(Utc::now()),
        }))
        .on_conflict(
            OnConflict::column(station::Column::StationAbbr)
                .update_columns([
                    station::Column::Name,
                    station::Column::DataSince,
                    station::Column::HeightMasl,
                    station::Column::HeightBarometerMasl,
                    station::Column::Latitude,
                    station::Column::Longitude,
                    station::Column::UpdatedAt,
                ])
                .to_owned(),
        )
        .exec_without_returning(db)
        .await?;
        written += usize::try_from(affected).unwrap_or(0);
    }
    Ok(written)
}

/// The attribution the MeteoSwiss terms ask for on anything published from the feed (Q77).
pub const ATTRIBUTION: &str = "Source: MeteoSwiss";

/// The feed attributing each of a site's parameters, keyed by catalog parameter. Read from the
/// subscription rows, so a parameter whose code merely looks like a MeteoSwiss one is not
/// attributed to the feed and a subscription switched off stops attributing.
#[must_use]
pub fn attributions(subscriptions: &[subscription::Model]) -> HashMap<Uuid, ExternalSource> {
    subscriptions
        .iter()
        .filter(|sub| sub.enabled)
        .map(|sub| {
            (
                sub.parameter_id,
                ExternalSource {
                    system: SOURCE_SYSTEM.to_string(),
                    station: sub.station_abbr.trim().to_uppercase(),
                    attribution: ATTRIBUTION.to_string(),
                },
            )
        })
        .collect()
}

/// The attributions for one site, for a reader building its parameter list.
pub async fn site_attributions<C: ConnectionTrait>(
    db: &C,
    site_id: Uuid,
) -> Result<HashMap<Uuid, ExternalSource>, DbErr> {
    let subscriptions = subscription::Entity::find()
        .filter(subscription::Column::SiteId.eq(site_id))
        .filter(subscription::Column::Enabled.eq(true))
        .all(db)
        .await?;
    Ok(attributions(&subscriptions))
}

/// Mean Earth radius, the one the great-circle distance is quoted against.
const EARTH_RADIUS_KM: f64 = 6371.0088;

/// The stations whose abbreviation or name carries `q`, or every station where nothing was typed.
pub async fn search_stations<C: ConnectionTrait>(
    db: &C,
    q: Option<&str>,
) -> Result<Vec<super::models::station::Model>, DbErr> {
    use sea_orm::sea_query::extension::postgres::PgExpr;
    use sea_orm::sea_query::{Expr, ExprTrait};

    use super::models::station;
    let mut query = station::Entity::find();
    if let Some(term) = q.map(str::trim).filter(|t| !t.is_empty()) {
        // A picker is typed into in any case, and the wildcards are the operator's text rather
        // than a pattern they wrote.
        let pattern = format!("%{}%", term.replace('%', "\\%").replace('_', "\\_"));
        query = query.filter(
            Expr::col(station::Column::StationAbbr)
                .ilike(&pattern)
                .or(Expr::col(station::Column::Name).ilike(&pattern)),
        );
    }
    query.all(db).await
}

/// The point a site ranks stations from, where it has one. Coordinates are hand-entered, so a site
/// without them is ordinary rather than an error.
pub async fn site_origin<C: ConnectionTrait>(
    db: &C,
    site_id: Uuid,
) -> Result<Option<(f64, f64)>, DbErr> {
    let Some(site) = crate::routes::private::sites::Entity::find_by_id(site_id)
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    Ok(site.latitude.zip(site.longitude))
}

/// Great-circle distance in kilometres between two WGS84 points.
#[must_use]
pub fn distance_km(from: (f64, f64), to: (f64, f64)) -> f64 {
    let (lat1, lon1) = (from.0.to_radians(), from.1.to_radians());
    let (lat2, lon2) = (to.0.to_radians(), to.1.to_radians());
    let half_dlat = ((lat2 - lat1) / 2.0).sin();
    let half_dlon = ((lon2 - lon1) / 2.0).sin();
    let a = half_dlat.mul_add(half_dlat, lat1.cos() * lat2.cos() * half_dlon * half_dlon);
    2.0 * EARTH_RADIUS_KM * a.sqrt().asin()
}

/// Order the candidates for a picker: nearest first from the site's coordinates, and by name where
/// the site has none or a station's own coordinates are missing. The published list is 158 rows,
/// so the ordering is done over the rows rather than asked of the database.
#[must_use]
pub fn rank_stations(
    stations: Vec<super::models::station::Model>,
    origin: Option<(f64, f64)>,
    declared: Option<&Variable>,
) -> Vec<StationCandidate> {
    let mut candidates: Vec<StationCandidate> = stations
        .into_iter()
        .map(|station| {
            let distance_km = origin
                .zip(station.latitude.zip(station.longitude))
                .map(|(origin, at)| distance_km(origin, at));
            let published = declared.map(|v| publishes(v, &station));
            StationCandidate {
                station_abbr: station.station_abbr,
                name: station.name,
                data_since: station.data_since,
                height_masl: station.height_masl,
                height_barometer_masl: station.height_barometer_masl,
                publishes: published,
                latitude: station.latitude,
                longitude: station.longitude,
                distance_km,
            }
        })
        .collect();
    candidates.sort_by(|a, b| match (a.distance_km, b.distance_km) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.name.cmp(&b.name),
    });
    candidates
}

/// Every enabled subscription, ordered so a station's sites are read together.
pub async fn subscribers<C: ConnectionTrait>(db: &C) -> Result<Vec<Subscriber>, DbErr> {
    Subscriber::find_by_statement(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT sub.id AS subscription_id, sub.site_id, s.name AS site_name,
                upper(btrim(sub.station_abbr)) AS station,
                lower(btrim(sub.variable)) AS variable,
                sub.parameter_id
               FROM meteoswiss_subscriptions sub
               JOIN sites s ON s.id = sub.site_id
              WHERE sub.enabled
              ORDER BY station, variable, s.name",
    ))
    .all(db)
    .await
}

/// `lower(code) = $1`, the shape of the catalog's unique index on the code.
fn code_matches(code: &str) -> sea_orm::sea_query::SimpleExpr {
    use sea_orm::sea_query::{Expr, ExprTrait, Func};
    Expr::expr(Func::lower(Expr::col(parameters::Column::Code))).eq(code)
}

/// The catalog row a variable lands on, minted on the subscription's own transaction where the
/// catalog does not hold it yet.
pub async fn catalog_parameter<C: ConnectionTrait>(
    db: &C,
    variable: &Variable,
) -> Result<Uuid, DbErr> {
    if let Some(existing) = parameters::Entity::find()
        .filter(code_matches(variable.code))
        .one(db)
        .await?
    {
        return Ok(existing.id);
    }
    Ok(parameters::ActiveModel {
        id: Set(Uuid::new_v4()),
        code: Set(variable.code.to_string()),
        name: Set(variable.label.to_string()),
        default_units: Set(variable.units.to_string()),
        category: Set("measurement".to_string()),
        description: Set(Some(format!(
            "MeteoSwiss SMN {}, landed per subscribed station",
            variable.name
        ))),
        ..Default::default()
    }
    .insert(db)
    .await?
    .id)
}

/// The station as an instrument: one row per station, shared by every site that reads it.
///
/// Registered on `(source_system, source_key)`, which is unique only where both are set: an
/// instrument named by serial carries neither, and the registration's predicate is what keeps
/// every one of those from reading as the same row.
pub async fn instrument<C: ConnectionTrait + sea_orm::TransactionTrait>(
    db: &C,
    station: &str,
) -> Result<Uuid, DbErr> {
    let mut mint = sensors::ActiveModel {
        name: Set(Some(format!("MeteoSwiss {station}"))),
        manufacturer: Set(Some("MeteoSwiss".to_string())),
        model: Set(Some("SMN".to_string())),
        kind: Set("device".to_string()),
        data_frequency: Set("high".to_string()),
        metadata: Set(Some(serde_json::json!({
            "station_abbr": station,
        }))),
        ..Default::default()
    };
    mint.source_system = Set(Some(SOURCE_SYSTEM.to_string()));
    mint.source_key = Set(Some(station.to_string()));
    let (sensor, _) = crudcrate::upsert::<sensors::Sensor, _>(db, mint)
        .await
        .map_err(|e| {
            DbErr::Custom(format!(
                "Failed to mint the MeteoSwiss station instrument: {e}"
            ))
        })?;
    Ok(sensor.id)
}

/// The site's slot for the subscribed variable and the stream feeding it, created with the
/// subscription. Subscribing the site to a station and a variable is the whole operator action;
/// the slot and the stream follow from it, and a later pass finds both and creates nothing.
pub async fn provision<C: ConnectionTrait + sea_orm::TransactionTrait>(
    db: &C,
    site: &Subscriber,
    parameter_id: Uuid,
) -> Result<Uuid, DbErr> {
    let declared = variable(&site.variable);
    let existing = site_parameters::Entity::find()
        .filter(site_parameters::Column::SiteId.eq(site.site_id))
        .filter(site_parameters::Column::ParameterId.eq(parameter_id))
        .one(db)
        .await?;
    let site_parameter_id = match existing {
        Some(slot) => slot.id,
        None => {
            // The slot is named and measured in whatever the catalog row says the variable is.
            let parameter = parameters::Entity::find_by_id(parameter_id)
                .one(db)
                .await?
                .ok_or_else(|| DbErr::Custom("The subscribed parameter is gone".to_string()))?;
            site_parameters::ActiveModel {
                id: Set(Uuid::new_v4()),
                site_id: Set(site.site_id),
                parameter_id: Set(parameter_id),
                name: Set(format!("{} {}", site.site_name, parameter.name)),
                decimal_places: Set(declared.map(|v| v.decimals)),
                sample_interval_sec: Set(Some(600)),
                // An operator picked the station, so there is nothing here for a manager to
                // confirm; the flag is for a column a tool save added.
                needs_review: Set(false),
                // The subscription registers its stream `continuous`, so the slot it fills is the
                // stream arm's.
                cadence: Set("high".to_string()),
                ..Default::default()
            }
            .insert(db)
            .await?
            .id
        }
    };

    let source_key = format!("{}:{}:{}", site.station, site.variable, site.site_id);
    let mut register = data_streams::ActiveModel {
        source_name: Set(Some(format!("{} {}", site.station, site.variable))),
        site_parameter_id: Set(Some(site_parameter_id)),
        measurement_type: Set(Some("continuous".to_string())),
        metadata: Set(serde_json::json!({
            "station": site.station,
            "variable": site.variable,
            "decimal_places": declared.map(|v| v.decimals),
        })),
        ..Default::default()
    };
    register.source_system = Set(SOURCE_SYSTEM.to_string());
    register.source_key = Set(source_key);
    let (stream, _) = crudcrate::upsert::<data_streams::DataStream, _>(db, register)
        .await
        .map_err(|e| DbErr::Custom(format!("Failed to register the MeteoSwiss stream: {e}")))?;

    // `paired_at` records when the stream first gained its slot, so it is stamped once and never
    // moved. Leaving it off the registration is what keeps a later pass from re-stamping it.
    if stream.paired_at.is_none() {
        data_streams::Entity::update_many()
            .col_expr(
                data_streams::Column::PairedAt,
                sea_orm::sea_query::Expr::current_timestamp(),
            )
            .filter(data_streams::Column::Id.eq(stream.id))
            .filter(data_streams::Column::PairedAt.is_null())
            .exec(db)
            .await?;
    }
    Ok(stream.id)
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
    crate::routes::private::data_streams::service::advance_cursor(stream_id, newest.into())
        .exec(db)
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

/// What a land owes the rest of the system: the slot and channel it wrote to, and how many rows.
///
/// `DataIngested` is the whole cache-invalidation contract (`common/cache.rs`), and an open site
/// page refreshes on it, so a pass that writes readings and stays quiet leaves both serving the
/// answer from before it ran. A pass that wrote nothing has nothing to invalidate.
#[must_use]
pub fn announcement(
    site_id: Uuid,
    parameter_id: Uuid,
    stream_id: Uuid,
    written: usize,
) -> Option<crate::common::AppEvent> {
    (written > 0).then_some(crate::common::AppEvent::DataIngested {
        site_id: Some(site_id),
        parameter_id: Some(parameter_id),
        stream_id: Some(stream_id),
        count: written,
    })
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

/// The ETag the last fetch of this URL returned, if it returned one.
pub async fn stored_etag<C: ConnectionTrait>(db: &C, url: &str) -> Result<Option<String>, DbErr> {
    Ok(
        super::models::fetch_state::Entity::find_by_id(url.to_string())
            .one(db)
            .await?
            .and_then(|row| row.etag),
    )
}

/// Record what a fetch of this URL returned. A 304 is a fetch too: it moves `fetched_at` and keeps
/// the ETag that produced it.
pub async fn record_fetch<C: ConnectionTrait>(
    db: &C,
    url: &str,
    etag: Option<String>,
) -> Result<(), DbErr> {
    let row = super::models::fetch_state::ActiveModel {
        url: Set(url.to_string()),
        etag: Set(etag),
        fetched_at: Set(Utc::now()),
    };
    super::models::fetch_state::Entity::insert(row)
        .on_conflict(
            OnConflict::column(super::models::fetch_state::Column::Url)
                .update_columns([
                    super::models::fetch_state::Column::Etag,
                    super::models::fetch_state::Column::FetchedAt,
                ])
                .to_owned(),
        )
        .exec_without_returning(db)
        .await?;
    Ok(())
}

/// One conditional GET: the stored ETag goes out as `If-None-Match`, a 304 says the source holds
/// what we hold, and whatever comes back is recorded against the URL for the next fetch.
///
/// MeteoSwiss publish no polling limit and ask for this instead, so it is how every fetch here is
/// made rather than an optimisation on one of them.
pub async fn fetch<C: ConnectionTrait>(
    db: &C,
    client: &reqwest::Client,
    url: &str,
) -> Result<Fetched, String> {
    let mut request = client.get(url);
    if let Some(etag) = stored_etag(db, url).await.map_err(|e| e.to_string())? {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    if status == reqwest::StatusCode::NOT_MODIFIED {
        record_fetch(db, url, etag)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(Fetched::Unchanged);
    }
    if !status.is_success() {
        return Err(format!("{url}: HTTP {status}"));
    }
    // The OGD files are latin-1: the station metadata carries names like Oberägeri, and reading
    // them as UTF-8 replaces the byte rather than the character. The data files are ASCII, which
    // latin-1 reads identically.
    let body = latin1(&response.bytes().await.map_err(|e| e.to_string())?);
    record_fetch(db, url, etag)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Fetched::Body(body))
}

/// Latin-1 is a byte-per-character mapping onto the first 256 code points, so it always decodes.
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|b| char::from(*b)).collect()
}

/// The subscription's hooks: a site subscribes to a declared variable, and the catalog row that
/// variable lands on is minted with the subscription rather than left for somebody to create.
pub struct MeteoswissSubscriptionOperations;

impl crudcrate::CRUDOperations for MeteoswissSubscriptionOperations {
    type Resource = super::models::subscription::MeteoswissSubscription;

    /// A variable the feed does not publish lands nowhere, a station it does not list is read for
    /// ever at a 404, and a station that publishes no such measurement is read for ever at blank
    /// cells, so all three are refused by name.
    async fn before_create<C: ConnectionTrait + sea_orm::TransactionTrait>(
        &self,
        db: &C,
        data: &<Self::Resource as crudcrate::CRUDResource>::CreateModel,
    ) -> Result<(), crudcrate::ApiError> {
        require_declared(&data.variable)?;
        require_station_publishes(db, &data.station_abbr, &data.variable).await
    }

    /// The catalog row is the subscription's own: it is resolved, minted where the catalog does
    /// not hold it, and written with the row rather than asked for.
    async fn perform_create<C: ConnectionTrait + sea_orm::TransactionTrait>(
        &self,
        db: &C,
        data: <Self::Resource as crudcrate::CRUDResource>::CreateModel,
    ) -> Result<Self::Resource, crudcrate::ApiError> {
        let declared = declaration(&data.variable)?;
        let parameter_id = catalog_parameter(db, declared).await?;
        let mut active: super::models::subscription::ActiveModel = data.into();
        active.parameter_id = Set(parameter_id);
        let subscription = active
            .insert(db)
            .await
            .map(Self::Resource::from)
            .map_err(crudcrate::ApiError::database)?;
        let site = subscriber(db, &subscription).await?;
        provision(db, &site, parameter_id)
            .await
            .map_err(crudcrate::ApiError::database)?;
        enqueue_backfill(db, &subscription.station_abbr, &subscription.variable).await?;
        Ok(subscription)
    }

    /// A subscription moved to another station or another variable is held to the same three
    /// refusals as a new one, over whichever half of the pair the update leaves alone.
    async fn before_update<C: ConnectionTrait + sea_orm::TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
        data: &<Self::Resource as crudcrate::CRUDResource>::UpdateModel,
    ) -> Result<(), crudcrate::ApiError> {
        let moved_variable = data.variable.as_ref().and_then(Option::as_ref);
        let moved_station = data.station_abbr.as_ref().and_then(Option::as_ref);
        if let Some(name) = moved_variable {
            require_declared(name)?;
        }
        if moved_variable.is_none() && moved_station.is_none() {
            return Ok(());
        }
        let (station, variable) = subscribed_pair(db, id).await?;
        let station = moved_station.unwrap_or(&station);
        let variable = moved_variable.unwrap_or(&variable);
        require_station_publishes(db, station, variable).await
    }

    /// A subscription moved to another variable lands on that variable's catalog row.
    async fn after_update<C: ConnectionTrait + sea_orm::TransactionTrait>(
        &self,
        db: &C,
        entity: &mut Self::Resource,
    ) -> Result<(), crudcrate::ApiError> {
        let declared = declaration(&entity.variable)?;
        let parameter_id = catalog_parameter(db, declared).await?;
        if entity.parameter_id == parameter_id {
            return Ok(());
        }
        super::models::subscription::Entity::update_many()
            .col_expr(
                super::models::subscription::Column::ParameterId,
                sea_orm::sea_query::Expr::value(parameter_id),
            )
            .filter(super::models::subscription::Column::Id.eq(entity.id))
            .exec(db)
            .await?;
        entity.parameter_id = parameter_id;
        Ok(())
    }
}

/// The history a subscription needs, once per station and variable however many sites read it.
///
/// The recurring pass starts at the stream's cursor, so a subscription made today would otherwise
/// begin today and a grab sample from 2018 would have no pressure at its instant for ever.
async fn enqueue_backfill<C: ConnectionTrait>(
    db: &C,
    station: &str,
    variable: &str,
) -> Result<(), crudcrate::ApiError> {
    let station = station.trim().to_uppercase();
    let variable = variable.trim().to_lowercase();
    let key = format!("meteoswiss_backfill:{station}:{variable}");
    jobs::enqueue(
        db,
        "meteoswiss_backfill",
        None,
        None,
        &serde_json::json!({ "station": station, "variable": variable }),
        Some(&key),
    )
    .await
    .map_err(crudcrate::ApiError::database)?;
    Ok(())
}

/// The subscription as the landing reads it, so the slot and the stream are provisioned in the
/// same shape whichever end asks for them.
async fn subscriber<C: ConnectionTrait>(
    db: &C,
    subscription: &super::models::subscription::MeteoswissSubscription,
) -> Result<Subscriber, crudcrate::ApiError> {
    let site = crate::routes::private::sites::Entity::find_by_id(subscription.site_id)
        .one(db)
        .await
        .map_err(crudcrate::ApiError::database)?
        .ok_or_else(|| crudcrate::ApiError::not_found("site", None))?;
    Ok(Subscriber {
        subscription_id: subscription.id,
        site_id: subscription.site_id,
        site_name: site.name,
        station: subscription.station_abbr.trim().to_uppercase(),
        variable: subscription.variable.trim().to_lowercase(),
        parameter_id: subscription.parameter_id,
    })
}

/// The station and variable a subscription holds now, so an update moving one is checked against
/// the other as it stands.
async fn subscribed_pair<C: ConnectionTrait>(
    db: &C,
    id: Uuid,
) -> Result<(String, String), crudcrate::ApiError> {
    subscription::Entity::find_by_id(id)
        .one(db)
        .await
        .map_err(crudcrate::ApiError::database)?
        .map(|row| (row.station_abbr, row.variable))
        .ok_or_else(|| crudcrate::ApiError::not_found("MeteoSwiss subscription", None))
}

/// What a backfill that landed nothing has to say about it. A run that read at least one archive
/// and found every cell blank has read the station's own answer: it publishes no such measurement
/// for the interval, which is a failed run rather than a quiet one. A run that read no archive has
/// nothing to conclude from, and one that landed a reading succeeded.
#[must_use]
pub fn nothing_published(
    station: &str,
    variable: &str,
    archives_read: usize,
    blank: usize,
    inserted: usize,
) -> Option<String> {
    if archives_read == 0 || inserted > 0 || blank == 0 {
        return None;
    }
    Some(format!(
        "{station} publishes no {variable} in {archives_read} archives: {blank} cells, none with a value"
    ))
}

/// The declaration for a subscribed variable, as a refusal where the feed publishes no such thing.
fn declaration(name: &str) -> Result<&'static Variable, crudcrate::ApiError> {
    variable(name).ok_or_else(|| undeclared(name))
}

fn require_declared(name: &str) -> Result<(), crudcrate::ApiError> {
    declaration(name).map(|_| ())
}

/// A station out of the published list that publishes the variable, read from the list as it
/// stands.
async fn require_station_publishes<C: ConnectionTrait>(
    db: &C,
    abbr: &str,
    variable: &str,
) -> Result<(), crudcrate::ApiError> {
    let listed = search_stations(db, None)
        .await
        .map_err(crudcrate::ApiError::database)?;
    let declared = declaration(variable)?;
    listed_station(abbr, &listed)?;
    station_publishes(abbr, declared, &listed)
}

/// The abbreviations a refusal offers instead of the one that was typed.
const NEAREST_STATIONS: usize = 3;

/// A subscription names a station the published list holds. The list is maintained from the
/// metadata file on every pass and is empty until the first one, where an abbreviation passes
/// rather than the feed's absence standing between an operator and a subscription.
fn listed_station(abbr: &str, listed: &[station::Model]) -> Result<(), crudcrate::ApiError> {
    let typed = abbr.trim().to_uppercase();
    let held = listed
        .iter()
        .any(|station| station.station_abbr.trim().to_uppercase() == typed);
    if listed.is_empty() || held {
        return Ok(());
    }
    Err(unlisted(&typed, listed))
}

/// A subscription names a station the list marks as publishing the variable. As with the list
/// itself, a station the list does not hold at all is another refusal's business, and an empty
/// list stands between nobody and a subscription.
fn station_publishes(
    abbr: &str,
    declared: &Variable,
    listed: &[station::Model],
) -> Result<(), crudcrate::ApiError> {
    let typed = abbr.trim().to_uppercase();
    let Some(station) = listed
        .iter()
        .find(|station| station.station_abbr.trim().to_uppercase() == typed)
    else {
        return Ok(());
    };
    if publishes(declared, station) {
        return Ok(());
    }
    Err(unpublished(&typed, declared, listed))
}

fn unpublished(typed: &str, declared: &Variable, listed: &[station::Model]) -> crudcrate::ApiError {
    let publishing: Vec<&station::Model> = listed
        .iter()
        .filter(|station| publishes(declared, station))
        .collect();
    let nearest: Vec<String> = nearest_of(typed, &publishing)
        .into_iter()
        .map(|station| format!("{} ({})", station.station_abbr, station.name))
        .collect();
    crudcrate::ApiError::bad_request(format!(
        "MeteoSwiss station '{typed}' publishes no {}; nearest that does: {}",
        declared.name,
        nearest.join(", ")
    ))
}

fn unlisted(typed: &str, listed: &[station::Model]) -> crudcrate::ApiError {
    let nearest: Vec<String> = nearest_stations(typed, listed)
        .into_iter()
        .map(|station| format!("{} ({})", station.station_abbr, station.name))
        .collect();
    crudcrate::ApiError::bad_request(format!(
        "MeteoSwiss list no station '{typed}'; nearest: {}",
        nearest.join(", ")
    ))
}

/// The listed stations closest to what was typed, by edit distance on the abbreviation and then by
/// name, so a refusal carries the one the operator meant.
fn nearest_stations<'a>(typed: &str, listed: &'a [station::Model]) -> Vec<&'a station::Model> {
    let all: Vec<&station::Model> = listed.iter().collect();
    nearest_of(typed, &all)
}

/// The same ranking over a chosen few, so a refusal can offer only the stations that qualify.
fn nearest_of<'a>(typed: &str, candidates: &[&'a station::Model]) -> Vec<&'a station::Model> {
    let mut by_distance: Vec<(usize, &station::Model)> = candidates
        .iter()
        .map(|station| {
            (
                edit_distance(typed, &station.station_abbr.trim().to_uppercase()),
                *station,
            )
        })
        .collect();
    by_distance.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
    by_distance
        .into_iter()
        .take(NEAREST_STATIONS)
        .map(|(_, station)| station)
        .collect()
}

/// Levenshtein distance, over the two abbreviations a typo separates.
fn edit_distance(from: &str, to: &str) -> usize {
    let to: Vec<char> = to.chars().collect();
    let mut row: Vec<usize> = (0..=to.len()).collect();
    for (i, a) in from.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, b) in to.iter().enumerate() {
            let cost = usize::from(a != *b);
            let replace = diagonal + cost;
            diagonal = row[j + 1];
            row[j + 1] = replace.min(row[j] + 1).min(diagonal + 1);
        }
    }
    row[to.len()]
}

fn undeclared(name: &str) -> crudcrate::ApiError {
    let declared: Vec<&str> = VARIABLES.iter().map(|v| v.name).collect();
    crudcrate::ApiError::bad_request(format!(
        "MeteoSwiss publishes no variable '{name}' here; declared: {}",
        declared.join(", ")
    ))
}

#[cfg(test)]
#[path = "tests/service.rs"]
mod tests;
