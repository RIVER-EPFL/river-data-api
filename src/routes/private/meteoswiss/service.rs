//! Reading the SMN feed and landing it: the conditional fetch, the CSV parser, and every query the
//! sync makes.
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

use super::models::{Fetched, Point, Series, Subscriber};
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
}

/// What a site may subscribe to. Station-level pressure is the variable the oxygen-saturation
/// procedure needs; another is added here with the catalog row it mints.
pub const VARIABLES: &[Variable] = &[Variable {
    name: "prestas0",
    code: "barometric_pressure",
    label: "Barometric Pressure",
    units: "hPa",
    decimals: 1,
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

/// The site's slot for the subscribed variable and the stream feeding it, created on first sync.
/// Subscribing the site to a station and a variable is the whole operator action; the slot and the
/// stream follow from it.
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
                display_units: Set(Some(parameter.default_units)),
                decimal_places: Set(declared.map(|v| v.decimals)),
                sample_interval_sec: Set(Some(600)),
                needs_review: Set(true),
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
    let body = response.text().await.map_err(|e| e.to_string())?;
    record_fetch(db, url, etag)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Fetched::Body(body))
}

/// The subscription's hooks: a site subscribes to a declared variable, and the catalog row that
/// variable lands on is minted with the subscription rather than left for somebody to create.
pub struct MeteoswissSubscriptionOperations;

impl crudcrate::CRUDOperations for MeteoswissSubscriptionOperations {
    type Resource = super::models::subscription::MeteoswissSubscription;

    /// A variable the feed does not publish lands nowhere, so it is refused by name.
    async fn before_create<C: ConnectionTrait + sea_orm::TransactionTrait>(
        &self,
        _db: &C,
        data: &<Self::Resource as crudcrate::CRUDResource>::CreateModel,
    ) -> Result<(), crudcrate::ApiError> {
        require_declared(&data.variable)
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
        active
            .insert(db)
            .await
            .map(Self::Resource::from)
            .map_err(crudcrate::ApiError::database)
    }

    async fn before_update<C: ConnectionTrait + sea_orm::TransactionTrait>(
        &self,
        _db: &C,
        _id: Uuid,
        data: &<Self::Resource as crudcrate::CRUDResource>::UpdateModel,
    ) -> Result<(), crudcrate::ApiError> {
        match data.variable.as_ref().and_then(Option::as_ref) {
            Some(name) => require_declared(name),
            None => Ok(()),
        }
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

/// The declaration for a subscribed variable, as a refusal where the feed publishes no such thing.
fn declaration(name: &str) -> Result<&'static Variable, crudcrate::ApiError> {
    variable(name).ok_or_else(|| undeclared(name))
}

fn require_declared(name: &str) -> Result<(), crudcrate::ApiError> {
    declaration(name).map(|_| ())
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
