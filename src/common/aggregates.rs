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
//!
//! Nothing here fills the head of a rollup or repairs its history on a schedule. The views are
//! real-time, so the open bucket is read from the raw rows, and each policy starts at NULL, so a
//! change to an old reading is materialised again by the next tick. A refresh here is a caller
//! making its own change visible before that tick, over the span it moved and no more.

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
}

/// What a refresh should cover. The head of every rollup is served from the raw rows and each
/// view's policy starts at NULL, so a window here is only about making a change visible before
/// the next tick would carry it; there is no whole-history shape, because that is the policies'.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
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
    fn bounds(self, now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
        match self {
            Window::Since(since) => (since, now.max(since)),
            Window::Range(a, b) => (a.min(b), a.max(b)),
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

    for resolution in Resolution::ALL {
        let statement = refresh_statement(resolution, window, now)?;
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

/// The `CALL` for one view, with the window aligned to that view's buckets.
///
/// The one statement in this crate that is not built through crudcrate or `sea_query`, because
/// neither expresses `CALL` (Q146). What keeps it safe is where its parts come from: the instants
/// are bound, and the view name is [`Resolution::view`], never a string a caller supplied.
fn refresh_statement(
    resolution: Resolution,
    window: Window,
    now: DateTime<Utc>,
) -> AppResult<Statement> {
    let view = resolution.view();
    let (lo, hi) = window.bounds(now);
    let start = resolution.floor(lo)?;
    let end = resolution.bucket_end(hi.max(lo))?;
    Ok(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        format!("CALL refresh_continuous_aggregate('{view}', $1::timestamptz, $2::timestamptz)"),
        [start.into(), end.into()],
    ))
}

#[cfg(test)]
#[path = "tests/aggregates.rs"]
mod tests;
