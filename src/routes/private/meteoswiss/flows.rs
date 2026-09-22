//! The scheduled fetches: the ten-minute pass over the all-stations latest-values file, and the
//! daily pass over each subscribed station's recent archive that fills what downtime missed.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sea_orm::DbErr;
use std::collections::BTreeMap;

use super::models::{Fetched, Point, Series, Subscriber};
use super::service::{
    advance_cursor, announcement, archive_hrefs, archives_since, cursor, fetch, historical, insert,
    instrument, latest, nothing_published, provision, recent_url, series, site_floor,
    stac_item_url, stations, stations_url, store_stations, subscribers,
};
use crate::config::Config;
use crate::routes::private::reprocessing_jobs::service::{Job, JobContext, JobReport, Schedule};

/// What a pass landed, so the two jobs report the same numbers under the same names.
#[derive(Default)]
struct Landed {
    inserted: usize,
    earliest: Option<DateTime<Utc>>,
}

impl Landed {
    fn saw(&mut self, oldest: Option<DateTime<Utc>>) {
        self.earliest = match (self.earliest, oldest) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }
}

/// The subscriptions of one pass, grouped station then variable: one fetch per station in the
/// archive pass, and one lookup per station in the latest pass.
type ByStation = BTreeMap<String, BTreeMap<String, Vec<Subscriber>>>;

fn by_station(subscribers: Vec<Subscriber>) -> ByStation {
    let mut grouped: ByStation = BTreeMap::new();
    for subscriber in subscribers {
        grouped
            .entry(subscriber.station.clone())
            .or_default()
            .entry(subscriber.variable.clone())
            .or_default()
            .push(subscriber);
    }
    grouped
}

fn http_client(timeout_seconds: u64) -> Result<reqwest::Client, DbErr> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_seconds.max(1)))
        .build()
        .map_err(|e| DbErr::Custom(format!("Failed to build the MeteoSwiss client: {e}")))
}

/// Which of a file's points a pass lands. A backfill ignores the cursor, because its points are
/// older than the cursor by construction, and reads back to the site's own earliest data and no
/// further: an archive older than that is history nothing at the site will line a value up with.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Backlog {
    FromCursor,
    Everything,
    Since(DateTime<Utc>),
}

/// The line an archive read writes to the run's timeline: the file by its own name, and what came
/// out of it. A person watching a backfill reads it to see which of the station's files it is on.
fn archive_line(href: &str, series: &Series) -> String {
    let name = href.rsplit('/').next().unwrap_or(href);
    format!(
        "Read {name}: {} rows, {} blank, {} unreadable",
        series.points.len(),
        series.blank,
        series.unreadable
    )
}

/// What a backfill lands at one site from one archive. `None` is an archive the site reads nothing
/// out of: a decade file at a site that holds no data of its own, which wants no history, while
/// the recent file still lands there so a fresh subscription is visibly working.
fn backlog_for(floor: Option<DateTime<Utc>>, historical: bool) -> Option<Backlog> {
    match floor {
        Some(floor) => Some(Backlog::Since(floor)),
        None if historical => None,
        None => Some(Backlog::Everything),
    }
}

/// Land the points a pass read for one subscription.
async fn land(
    ctx: &JobContext,
    site: &Subscriber,
    station: &str,
    points: &[Point],
    landed: &mut Landed,
    backlog: Backlog,
) -> Result<usize, DbErr> {
    let stream_id = provision(ctx.db(), site, site.parameter_id).await?;
    let cursor = cursor(ctx.db(), stream_id).await?;
    let fresh: Vec<&Point> = points
        .iter()
        .filter(|p| match backlog {
            Backlog::FromCursor => cursor.is_none_or(|c| p.time > c),
            Backlog::Everything => true,
            Backlog::Since(floor) => p.time >= floor,
        })
        .collect();
    if fresh.is_empty() {
        return Ok(0);
    }
    let sensor_id = instrument(ctx.db(), station).await?;
    let written = insert(
        ctx.db(),
        stream_id,
        site.site_id,
        site.parameter_id,
        sensor_id,
        &fresh,
    )
    .await?;
    landed.inserted += written;
    if let Some(event) = announcement(site.site_id, site.parameter_id, stream_id, written) {
        let _ = ctx.events().send(event);
    }
    landed.saw(fresh.iter().map(|p| p.time).min());
    if let Some(newest) = fresh.iter().map(|p| p.time).max() {
        advance_cursor(ctx.db(), stream_id, newest).await?;
    }
    Ok(written)
}

/// A pressure series rolls up like any other continuous parameter, so a pass makes what it landed
/// visible over the span it moved.
async fn refresh(ctx: &JobContext, landed: &Landed) -> Result<(), DbErr> {
    if let Some(since) = landed.earliest {
        let report = crate::common::sync_state::refresh_continuous_aggregates(ctx.db(), since)
            .await
            .map_err(|e| DbErr::Custom(format!("Aggregate refresh failed: {e}")))?;
        ctx.info(&report.line()).await;
    }
    Ok(())
}

/// Read the all-stations latest-values file and land the newest interval at every subscribed site.
///
/// One conditional request covers every station, so the cadence is the file's own: it is
/// republished every ten minutes, and a tick that finds the same ETag costs nothing.
pub struct MeteoswissSync {
    base_url: String,
    latest_url: String,
    interval_seconds: u64,
    timeout_seconds: u64,
}

impl MeteoswissSync {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            base_url: config.meteoswiss_base_url.clone(),
            latest_url: config.meteoswiss_latest_url.clone(),
            interval_seconds: config.meteoswiss_interval_seconds,
            timeout_seconds: config.meteoswiss_timeout_seconds,
        }
    }
}

impl MeteoswissSync {
    /// Pull the published station list and maintain the table from it. A failure here is logged
    /// and does not stop the pass: the list is what a station is chosen out of, not what a
    /// subscribed station is read with.
    async fn refresh_stations(&self, ctx: &JobContext, client: &reqwest::Client) -> usize {
        let url = stations_url(&self.base_url);
        let body = match fetch(ctx.db(), client, &url).await {
            Ok(Fetched::Body(body)) => body,
            Ok(Fetched::Unchanged) => return 0,
            Err(e) => {
                ctx.log(
                    "warn",
                    "Could not read the MeteoSwiss station list",
                    serde_json::json!({ "url": url, "error": e }),
                )
                .await;
                return 0;
            }
        };
        let rows = match stations(&body) {
            Ok(rows) => rows,
            Err(e) => {
                ctx.log(
                    "warn",
                    "Could not read the MeteoSwiss station list",
                    serde_json::json!({ "url": url, "error": e }),
                )
                .await;
                return 0;
            }
        };
        match store_stations(ctx.db(), &rows).await {
            Ok(stored) => stored,
            Err(e) => {
                ctx.log(
                    "warn",
                    "Could not store the MeteoSwiss station list",
                    serde_json::json!({ "error": e.to_string() }),
                )
                .await;
                0
            }
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
            i64::try_from(self.interval_seconds.max(1)).unwrap_or(600),
        ))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let client = http_client(self.timeout_seconds)?;

        // The list is refreshed before the subscriptions are read, and whether or not there are
        // any: it is what a station is chosen out of, so a database with no subscription yet is
        // exactly the one that needs it.
        let stations_stored = self.refresh_stations(&ctx, &client).await;

        let subscriptions = subscribers(ctx.db()).await?;
        if subscriptions.is_empty() {
            ctx.report(
                JobReport::new()
                    .count("stations", 0usize)
                    .count("stations_listed", stations_stored),
            )
            .await;
            return Ok(0);
        }
        let body = match fetch(ctx.db(), &client, &self.latest_url).await {
            Ok(Fetched::Body(body)) => body,
            // The file has not been republished since the last tick, so every station still holds
            // the interval already landed.
            Ok(Fetched::Unchanged) => {
                ctx.report(
                    JobReport::new()
                        .count("unchanged", 1usize)
                        .count("stations_listed", stations_stored),
                )
                .await;
                return Ok(0);
            }
            Err(e) => return Err(DbErr::Custom(format!("MeteoSwiss latest values: {e}"))),
        };

        let mut landed = Landed::default();
        let mut stations_read = 0usize;
        let mut absent = 0usize;
        let mut variables_failed = 0usize;

        for (station, by_variable) in by_station(subscriptions) {
            if ctx.is_cancelled() {
                break;
            }
            stations_read += 1;
            for (variable, sites) in by_variable {
                let reported = match latest(&body, &variable) {
                    Ok(reported) => reported,
                    Err(e) => {
                        variables_failed += 1;
                        ctx.log(
                            "warn",
                            "Could not read a MeteoSwiss variable",
                            serde_json::json!({ "variable": variable, "error": e }),
                        )
                        .await;
                        continue;
                    }
                };
                let Some(point) = reported.get(&station) else {
                    absent += 1;
                    ctx.log(
                        "warn",
                        "A MeteoSwiss station carries no value for the interval",
                        serde_json::json!({ "station": station, "variable": variable }),
                    )
                    .await;
                    continue;
                };
                for site in sites {
                    land(
                        &ctx,
                        &site,
                        &station,
                        std::slice::from_ref(point),
                        &mut landed,
                        Backlog::FromCursor,
                    )
                    .await?;
                }
            }
        }

        refresh(&ctx, &landed).await?;
        ctx.report(
            JobReport::new()
                .scope_opt("since", landed.earliest.map(|t| t.to_rfc3339()))
                .count("stations", stations_read)
                .count("stations_listed", stations_stored)
                .count("stations_absent", absent)
                .count("variables_failed", variables_failed)
                .count("readings_inserted", landed.inserted),
        )
        .await;
        Ok(i64::try_from(landed.inserted).unwrap_or(i64::MAX))
    }
}

/// Read each subscribed station's recent archive and land what the ten-minute pass missed.
///
/// The archive is 4.2 MB per station and republished twice a day, so it is read once a day and
/// conditionally: it is the gap filler after downtime, not the feed.
pub struct MeteoswissRecent {
    base_url: String,
    interval_seconds: u64,
    timeout_seconds: u64,
}

impl MeteoswissRecent {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            base_url: config.meteoswiss_base_url.clone(),
            interval_seconds: config.meteoswiss_recent_interval_seconds,
            timeout_seconds: config.meteoswiss_timeout_seconds,
        }
    }
}

#[async_trait]
impl Job for MeteoswissRecent {
    fn name(&self) -> &'static str {
        "meteoswiss_recent"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(
            i64::try_from(self.interval_seconds.max(1)).unwrap_or(86_400),
        ))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let subscriptions = subscribers(ctx.db()).await?;
        if subscriptions.is_empty() {
            ctx.report(JobReport::new().count("stations", 0usize)).await;
            return Ok(0);
        }
        let client = http_client(self.timeout_seconds)?;

        let mut landed = Landed::default();
        let mut stations_read = 0usize;
        let mut stations_failed = 0usize;
        let mut unchanged = 0usize;
        let mut variables_failed = 0usize;
        let mut blank = 0usize;
        let mut unreadable = 0usize;

        for (station, by_variable) in by_station(subscriptions) {
            if ctx.is_cancelled() {
                break;
            }
            let url = recent_url(&self.base_url, &station);
            let body = match fetch(ctx.db(), &client, &url).await {
                Ok(Fetched::Body(body)) => body,
                Ok(Fetched::Unchanged) => {
                    unchanged += 1;
                    continue;
                }
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

            for (variable, sites) in by_variable {
                let series = match series(&body, &variable) {
                    Ok(series) => series,
                    Err(e) => {
                        variables_failed += 1;
                        ctx.log(
                            "warn",
                            "Could not read a MeteoSwiss variable",
                            serde_json::json!({ "station": station, "variable": variable, "error": e }),
                        )
                        .await;
                        continue;
                    }
                };
                blank += series.blank;
                unreadable += series.unreadable;
                for site in sites {
                    land(
                        &ctx,
                        &site,
                        &station,
                        &series.points,
                        &mut landed,
                        Backlog::FromCursor,
                    )
                    .await?;
                }
            }
        }

        refresh(&ctx, &landed).await?;
        ctx.report(
            JobReport::new()
                .scope_opt("since", landed.earliest.map(|t| t.to_rfc3339()))
                .count("stations", stations_read)
                .count("stations_failed", stations_failed)
                .count("stations_unchanged", unchanged)
                .count("variables_failed", variables_failed)
                .count("readings_inserted", landed.inserted)
                .count("blank_cells", blank)
                .count("unreadable_rows", unreadable),
        )
        .await;
        Ok(i64::try_from(landed.inserted).unwrap_or(i64::MAX))
    }
}

/// Read the archives a station publishes back to the earliest data its subscribed sites hold of
/// their own, and land them under the same streams, so a subscription made today carries the
/// history a correction of an old grab sample needs and no more: a decade the site was not
/// measured in is history nothing there will ever line a value up with.
///
/// Enqueued once per (station, variable) when a site subscribes, and rerunnable: the inserts do
/// nothing on conflict, so a second run re-reads the same files and writes only what is missing.
pub struct MeteoswissBackfill {
    stac_url: String,
    timeout_seconds: u64,
}

impl MeteoswissBackfill {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            stac_url: config.meteoswiss_stac_url.clone(),
            timeout_seconds: config.meteoswiss_timeout_seconds,
        }
    }
}

#[async_trait]
impl Job for MeteoswissBackfill {
    fn name(&self) -> &'static str {
        "meteoswiss_backfill"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let station = ctx
            .params()
            .get("station")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| DbErr::Custom("meteoswiss_backfill needs a station".to_string()))?
            .to_uppercase();
        let variable = ctx
            .params()
            .get("variable")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| DbErr::Custom("meteoswiss_backfill needs a variable".to_string()))?
            .to_lowercase();

        // Every site reading this station and variable takes the same files, so the archives are
        // read once and landed at each.
        let sites: Vec<Subscriber> = subscribers(ctx.db())
            .await?
            .into_iter()
            .filter(|s| s.station == station && s.variable == variable)
            .collect();
        if sites.is_empty() {
            ctx.report(JobReport::new().count("archives", 0usize)).await;
            return Ok(0);
        }

        let client = http_client(self.timeout_seconds)?;
        let item_url = stac_item_url(&self.stac_url, &station);
        let item = match fetch(ctx.db(), &client, &item_url).await {
            Ok(Fetched::Body(body)) => serde_json::from_str::<serde_json::Value>(&body)
                .map_err(|e| DbErr::Custom(format!("{item_url}: {e}")))?,
            // The item is unchanged since the last backfill of this station, and the archives it
            // lists are read below on their own ETags.
            Ok(Fetched::Unchanged) => serde_json::Value::Null,
            Err(e) => return Err(DbErr::Custom(format!("MeteoSwiss station item: {e}"))),
        };
        let hrefs = archive_hrefs(&item).map_err(|e| DbErr::Custom(format!("{item_url}: {e}")))?;

        // How far back each site reads, and the deepest of them, which is the only history any
        // archive is fetched for.
        let mut floors: BTreeMap<uuid::Uuid, Option<DateTime<Utc>>> = BTreeMap::new();
        for site in &sites {
            let floor = site_floor(ctx.db(), site.site_id).await?;
            floors.insert(site.site_id, floor);
        }
        let deepest = floors.values().copied().flatten().min();
        let hrefs = archives_since(hrefs, deepest);

        let mut landed = Landed::default();
        let mut per_site: BTreeMap<uuid::Uuid, usize> = BTreeMap::new();
        let mut archives_read = 0usize;
        let mut archives_failed = 0usize;
        let mut blank = 0usize;
        let mut unreadable = 0usize;

        let archive_count = hrefs.len();
        for (walked, href) in hrefs.into_iter().enumerate() {
            if ctx.is_cancelled() {
                break;
            }
            // The position in the station's archive list, so the bar has a denominator: an
            // archive skipped as unchanged or unreadable still advances the walk.
            ctx.set_step(walked + 1, archive_count).await;
            let historical = historical(&href);
            let body = match fetch(ctx.db(), &client, &href).await {
                Ok(Fetched::Body(body)) => body,
                Ok(Fetched::Unchanged) => continue,
                Err(e) => {
                    archives_failed += 1;
                    ctx.log(
                        "warn",
                        "Could not read a MeteoSwiss archive",
                        serde_json::json!({ "url": href, "error": e }),
                    )
                    .await;
                    continue;
                }
            };
            let series = match series(&body, &variable) {
                Ok(series) => series,
                Err(e) => {
                    archives_failed += 1;
                    ctx.log(
                        "warn",
                        "Could not read a MeteoSwiss archive",
                        serde_json::json!({ "url": href, "error": e }),
                    )
                    .await;
                    continue;
                }
            };
            archives_read += 1;
            blank += series.blank;
            unreadable += series.unreadable;
            ctx.info(&archive_line(&href, &series)).await;
            for site in &sites {
                let floor = floors.get(&site.site_id).copied().flatten();
                let Some(backlog) = backlog_for(floor, historical) else {
                    continue;
                };
                let written =
                    land(&ctx, site, &station, &series.points, &mut landed, backlog).await?;
                *per_site.entry(site.site_id).or_default() += written;
            }
        }

        for site in &sites {
            let written = per_site.get(&site.site_id).copied().unwrap_or_default();
            ctx.info(&format!("Landed {written} at {}", site.site_name))
                .await;
        }
        refresh(&ctx, &landed).await?;
        ctx.report(
            JobReport::new()
                .scope("station", station.clone())
                .scope("variable", variable.clone())
                .scope_opt("since", landed.earliest.map(|t| t.to_rfc3339()))
                .count("archives", archives_read)
                .count("archives_failed", archives_failed)
                .count("readings_inserted", landed.inserted)
                .count("blank_cells", blank)
                .count("unreadable_rows", unreadable),
        )
        .await;
        if let Some(said) =
            nothing_published(&station, &variable, archives_read, blank, landed.inserted)
        {
            return Err(DbErr::Custom(said));
        }
        Ok(i64::try_from(landed.inserted).unwrap_or(i64::MAX))
    }
}

#[cfg(test)]
#[path = "tests/flows.rs"]
mod tests;
