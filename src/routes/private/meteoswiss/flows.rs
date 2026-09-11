//! The scheduled fetch: one pass over every declared station, landing what each site named.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sea_orm::DbErr;
use std::collections::BTreeMap;

use super::models::{Point, Subscriber};
use super::service::{
    PARAMETER_CODE, VARIABLE, advance_cursor, cursor, insert, instrument, parameter_id, provision,
    recent_url, series, subscribers,
};
use crate::config::Config;
use crate::routes::private::reprocessing_jobs::service::{Job, JobContext, JobReport, Schedule};

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
            let url = recent_url(&self.base_url, &station);
            let series = match fetch(&client, &url)
                .await
                .and_then(|body| series(&body, VARIABLE).map_err(|e| format!("{url}: {e}")))
            {
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
                let fresh: Vec<&Point> = series
                    .points
                    .iter()
                    .filter(|p| cursor.is_none_or(|c| p.time > c))
                    .collect();
                if fresh.is_empty() {
                    continue;
                }
                let sensor_id = instrument(ctx.db(), &station).await?;
                let written = insert(
                    ctx.db(),
                    stream_id,
                    site.site_id,
                    parameter_id,
                    sensor_id,
                    &fresh,
                )
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
            crate::common::sync_state::refresh_continuous_aggregates(ctx.db(), since)
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
