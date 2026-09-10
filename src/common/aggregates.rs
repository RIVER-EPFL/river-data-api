//! The continuous-aggregate refresh.
//!
//! One fallible entry point, [`refresh`], owns the four rollup views, the per-view bucket arithmetic
//! and the error handling. A caller states the window it changed; this module turns that into a
//! window TimescaleDB accepts.
//!
//! Two rules make the difference between a refresh that lands and one that does not:
//!
//! - `refresh_continuous_aggregate` inscribes its window to whole buckets, so the start must be
//!   floored to the view's bucket boundary or the bucket holding the change is skipped.
//! - The inscribed window must cover at least one complete bucket, otherwise the call raises
//!   `refresh window too small`. Flooring the start and taking the end of the bucket holding the end
//!   guarantees it.
//!
//! The procedure has its own transaction control, so it cannot run inside a transaction block: pass
//! a `DatabaseConnection`, after any guarded write has committed.

use chrono::{DateTime, Datelike, Duration, DurationRound, Months, Utc, Weekday};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};

use super::bulk_write::TouchedRange;
use crate::error::{AppError, AppResult};

/// A rollup view and its bucket width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Hourly,
    Daily,
    Weekly,
    Monthly,
}

impl Resolution {
    /// Every rollup, coarsest last.
    pub const ALL: [Resolution; 4] = [
        Resolution::Hourly,
        Resolution::Daily,
        Resolution::Weekly,
        Resolution::Monthly,
    ];

    #[must_use]
    pub fn view(self) -> &'static str {
        match self {
            Resolution::Hourly => "readings_hourly",
            Resolution::Daily => "readings_daily",
            Resolution::Weekly => "readings_weekly",
            Resolution::Monthly => "readings_monthly",
        }
    }

    /// Start of the bucket holding `t`, matching `time_bucket` on a UTC-anchored timestamptz:
    /// the hour, the UTC day, the Monday of the week, the first of the month.
    ///
    /// `duration_trunc` anchors to the Unix epoch, a Thursday, so the weekly bucket is taken from
    /// the Monday of `t`'s week rather than truncated to seven days.
    pub fn floor(self, t: DateTime<Utc>) -> AppResult<DateTime<Utc>> {
        let floor = match self {
            Resolution::Hourly => t.duration_trunc(Duration::hours(1)),
            Resolution::Daily => t.duration_trunc(Duration::days(1)),
            Resolution::Weekly => {
                let monday = t.date_naive().week(Weekday::Mon).first_day();
                monday
                    .and_hms_opt(0, 0, 0)
                    .map(|d| d.and_utc())
                    .ok_or(chrono::RoundingError::TimestampExceedsLimit)
            }
            Resolution::Monthly => t
                .date_naive()
                .with_day(1)
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|d| d.and_utc())
                .ok_or(chrono::RoundingError::TimestampExceedsLimit),
        };
        floor.map_err(|e| {
            AppError::Internal(format!("no {} bucket floor for {t}: {e}", self.view()))
        })
    }

    /// Exclusive end of the bucket holding `t`, ie. the next bucket boundary strictly after `t`.
    pub fn bucket_end(self, t: DateTime<Utc>) -> AppResult<DateTime<Utc>> {
        let start = self.floor(t)?;
        match self {
            Resolution::Hourly => Ok(start + Duration::hours(1)),
            Resolution::Daily => Ok(start + Duration::days(1)),
            Resolution::Weekly => Ok(start + Duration::days(7)),
            Resolution::Monthly => start
                .checked_add_months(Months::new(1))
                .ok_or_else(|| AppError::Internal(format!("no monthly bucket after {start}"))),
        }
    }

    /// How far back a refresh with no stated window reaches for this view.
    fn default_lookback(self) -> Duration {
        match self {
            Resolution::Hourly => Duration::hours(24),
            Resolution::Daily => Duration::days(7),
            Resolution::Weekly => Duration::days(14),
            Resolution::Monthly => Duration::days(62),
        }
    }
}

/// What a refresh should cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    /// The whole history of every view (`NULL, NULL`). The repair backstop; expensive.
    Full,
    /// The rolling per-view defaults (24h hourly, 7d daily, 14d weekly, 62d monthly).
    Recent,
    /// From an instant that changed up to now.
    Since(DateTime<Utc>),
    /// An explicit range of changed instants. Bounds may arrive in either order.
    Range(DateTime<Utc>, DateTime<Utc>),
}

impl Window {
    /// The window a guarded write's [`TouchedRange`] implies, or `None` when it changed nothing.
    #[must_use]
    pub fn touched(touched: &TouchedRange) -> Option<Window> {
        touched.span().map(|(lo, hi)| Window::Range(lo, hi))
    }

    /// The raw instants this window covers, before per-view bucket alignment.
    fn bounds(self, now: DateTime<Utc>) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        match self {
            Window::Full => None,
            Window::Recent => Some((now, now)),
            Window::Since(since) => Some((since, now.max(since))),
            Window::Range(a, b) => Some((a.min(b), a.max(b))),
        }
    }
}

/// Refresh every rollup over `window`.
///
/// Each view is attempted even if an earlier one fails, so one broken view cannot leave the others
/// stale; the first error is returned once all four have been tried. A caller inside a tracked job
/// must propagate the error, a swallowed refresh reports a job as completed while the rollups still
/// serve the old numbers.
pub async fn refresh(db: &DatabaseConnection, window: Window) -> AppResult<()> {
    let now = Utc::now();
    let mut first_error = None;
    let mut failed = 0;

    // A view recreated WITH NO DATA holds only what the rolling window has touched since, and
    // materialized-only reads serve that absence as data. The scheduled Recent refresh is where
    // that state gets noticed: any view whose earliest bucket does not reach the earliest
    // qualifying reading escalates to a full refresh here, so a rebuilt rollup heals on the next
    // tick. A failed probe refreshes normally rather than blocking.
    let escalated: Vec<Resolution> = if matches!(window, Window::Recent) {
        match views_missing_history(db).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "Aggregate coverage probe failed; refreshing the rolling window only");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    for resolution in Resolution::ALL {
        let view_window = if escalated.contains(&resolution) {
            tracing::warn!(
                view = resolution.view(),
                "Rollup is missing its history; running a full refresh"
            );
            Window::Full
        } else {
            window
        };
        let statement = refresh_statement(resolution, view_window, now)?;
        match db.execute_raw(statement).await {
            Ok(_) => tracing::debug!(view = resolution.view(), "Continuous aggregate refreshed"),
            Err(e) => {
                tracing::warn!(view = resolution.view(), error = %e, "Failed to refresh continuous aggregate");
                failed += 1;
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }

    match first_error {
        None => Ok(()),
        Some(e) => {
            tracing::error!(failed, error = %e, "Continuous aggregate refresh failed");
            Err(e.into())
        }
    }
}

/// The rollups whose earliest bucket does not reach the earliest reading their shared population
/// filter admits. Empty when every view covers its history (the steady state, two cheap MIN
/// probes per tick).
async fn views_missing_history(db: &DatabaseConnection) -> AppResult<Vec<Resolution>> {
    let earliest = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT MIN(time) AS t FROM readings
             WHERE site_id IS NOT NULL AND replicate_index = 0
               AND is_flagged IS NOT TRUE AND measurement_type IS DISTINCT FROM 'spot'"
                .to_string(),
        ))
        .await?
        .and_then(|row| {
            row.try_get::<Option<sea_orm::prelude::DateTimeWithTimeZone>>("", "t")
                .ok()
                .flatten()
        });
    let Some(earliest) = earliest else {
        return Ok(Vec::new());
    };
    let earliest: DateTime<Utc> = earliest.with_timezone(&Utc);

    let mut missing = Vec::new();
    for resolution in Resolution::ALL {
        let min_bucket = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("SELECT MIN(bucket) AS b FROM {}", resolution.view()),
            ))
            .await?
            .and_then(|row| {
                row.try_get::<Option<sea_orm::prelude::DateTimeWithTimeZone>>("", "b")
                    .ok()
                    .flatten()
            });
        let floor = resolution.floor(earliest)?;
        let covered = min_bucket.is_some_and(|b| b.with_timezone(&Utc) <= floor);
        if !covered {
            missing.push(resolution);
        }
    }
    Ok(missing)
}

/// The `CALL` for one view, with the window aligned to that view's buckets.
fn refresh_statement(
    resolution: Resolution,
    window: Window,
    now: DateTime<Utc>,
) -> AppResult<Statement> {
    let view = resolution.view();
    Ok(match window.bounds(now) {
        None => Statement::from_string(
            DatabaseBackend::Postgres,
            format!("CALL refresh_continuous_aggregate('{view}', NULL, NULL)"),
        ),
        Some((lo, hi)) => {
            let lo = match window {
                Window::Recent => lo - resolution.default_lookback(),
                _ => lo,
            };
            let start = resolution.floor(lo)?;
            let end = resolution.bucket_end(hi.max(lo))?;
            Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                format!(
                    "CALL refresh_continuous_aggregate('{view}', $1::timestamptz, $2::timestamptz)"
                ),
                [start.into(), end.into()],
            )
        }
    })
}

#[cfg(test)]
#[path = "tests/aggregates.rs"]
mod tests;
