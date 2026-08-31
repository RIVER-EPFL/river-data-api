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

use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};

use super::bulk_write::TouchedRange;
use crate::error::AppResult;

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
    #[must_use]
    pub fn floor(self, t: DateTime<Utc>) -> DateTime<Utc> {
        let date = t.date_naive();
        match self {
            Resolution::Hourly => t
                .with_minute(0)
                .and_then(|t| t.with_second(0))
                .and_then(|t| t.with_nanosecond(0))
                .unwrap_or(t),
            Resolution::Daily => midnight(date.year(), date.month(), date.day()),
            Resolution::Weekly => {
                let back = i64::from(date.weekday().num_days_from_monday());
                let monday = date - Duration::days(back);
                midnight(monday.year(), monday.month(), monday.day())
            }
            Resolution::Monthly => midnight(date.year(), date.month(), 1),
        }
    }

    /// Exclusive end of the bucket holding `t`, ie. the next bucket boundary strictly after `t`.
    #[must_use]
    pub fn bucket_end(self, t: DateTime<Utc>) -> DateTime<Utc> {
        let start = self.floor(t);
        match self {
            Resolution::Hourly => start + Duration::hours(1),
            Resolution::Daily => start + Duration::days(1),
            Resolution::Weekly => start + Duration::days(7),
            Resolution::Monthly => {
                let (year, month) = if start.month() == 12 {
                    (start.year() + 1, 1)
                } else {
                    (start.year(), start.month() + 1)
                };
                midnight(year, month, 1)
            }
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

fn midnight(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, 0, 0, 0)
        .single()
        .unwrap_or_else(Utc::now)
}

/// What a refresh should cover.
#[derive(Debug, Clone, Copy)]
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
            tracing::warn!(view = resolution.view(), "Rollup is missing its history; running a full refresh");
            Window::Full
        } else {
            window
        };
        let statement = refresh_statement(resolution, view_window, now);
        match db.execute(statement).await {
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
        .query_one(Statement::from_string(
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
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("SELECT MIN(bucket) AS b FROM {}", resolution.view()),
            ))
            .await?
            .and_then(|row| {
                row.try_get::<Option<sea_orm::prelude::DateTimeWithTimeZone>>("", "b")
                    .ok()
                    .flatten()
            });
        let covered = min_bucket
            .is_some_and(|b| b.with_timezone(&Utc) <= resolution.floor(earliest));
        if !covered {
            missing.push(resolution);
        }
    }
    Ok(missing)
}

/// The `CALL` for one view, with the window aligned to that view's buckets.
fn refresh_statement(resolution: Resolution, window: Window, now: DateTime<Utc>) -> Statement {
    let view = resolution.view();
    match window.bounds(now) {
        None => Statement::from_string(
            DatabaseBackend::Postgres,
            format!("CALL refresh_continuous_aggregate('{view}', NULL, NULL)"),
        ),
        Some((lo, hi)) => {
            let lo = match window {
                Window::Recent => lo - resolution.default_lookback(),
                _ => lo,
            };
            let start = resolution.floor(lo);
            let end = resolution.bucket_end(hi.max(lo));
            Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                format!(
                    "CALL refresh_continuous_aggregate('{view}', $1::timestamptz, $2::timestamptz)"
                ),
                [start.into(), end.into()],
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn test_floor_hourly_on_the_bucket_edge_is_the_edge() {
        assert_eq!(
            Resolution::Hourly.floor(t("2026-08-12T14:00:00Z")),
            t("2026-08-12T14:00:00Z")
        );
    }

    #[test]
    fn test_floor_hourly_one_microsecond_after_the_edge() {
        assert_eq!(
            Resolution::Hourly.floor(t("2026-08-12T14:00:00.000001Z")),
            t("2026-08-12T14:00:00Z")
        );
    }

    #[test]
    fn test_floor_hourly_mid_bucket() {
        assert_eq!(
            Resolution::Hourly.floor(t("2026-08-12T14:22:33.456789Z")),
            t("2026-08-12T14:00:00Z")
        );
    }

    #[test]
    fn test_floor_daily_is_utc_midnight() {
        assert_eq!(
            Resolution::Daily.floor(t("2026-08-12T00:00:00Z")),
            t("2026-08-12T00:00:00Z")
        );
        assert_eq!(
            Resolution::Daily.floor(t("2026-08-12T00:00:00.000001Z")),
            t("2026-08-12T00:00:00Z")
        );
        assert_eq!(
            Resolution::Daily.floor(t("2026-08-12T23:59:59.999999Z")),
            t("2026-08-12T00:00:00Z")
        );
    }

    #[test]
    fn test_floor_weekly_is_the_monday() {
        // 2026-08-12 is a Wednesday; the week bucket starts Monday 2026-08-10.
        assert_eq!(
            Resolution::Weekly.floor(t("2026-08-12T14:22:00Z")),
            t("2026-08-10T00:00:00Z")
        );
        assert_eq!(
            Resolution::Weekly.floor(t("2026-08-10T00:00:00Z")),
            t("2026-08-10T00:00:00Z")
        );
        assert_eq!(
            Resolution::Weekly.floor(t("2026-08-10T00:00:00.000001Z")),
            t("2026-08-10T00:00:00Z")
        );
        // A Sunday belongs to the week that started six days earlier.
        assert_eq!(
            Resolution::Weekly.floor(t("2026-08-16T23:00:00Z")),
            t("2026-08-10T00:00:00Z")
        );
    }

    #[test]
    fn test_floor_monthly_is_the_first() {
        assert_eq!(
            Resolution::Monthly.floor(t("2026-08-12T14:22:00Z")),
            t("2026-08-01T00:00:00Z")
        );
        assert_eq!(
            Resolution::Monthly.floor(t("2026-08-01T00:00:00Z")),
            t("2026-08-01T00:00:00Z")
        );
        assert_eq!(
            Resolution::Monthly.floor(t("2026-08-01T00:00:00.000001Z")),
            t("2026-08-01T00:00:00Z")
        );
    }

    #[test]
    fn test_floor_before_the_epoch() {
        assert_eq!(
            Resolution::Daily.floor(t("1969-12-30T05:00:00Z")),
            t("1969-12-30T00:00:00Z")
        );
        // 1969-12-29 is a Monday.
        assert_eq!(
            Resolution::Weekly.floor(t("1969-12-30T05:00:00Z")),
            t("1969-12-29T00:00:00Z")
        );
    }

    #[test]
    fn test_bucket_end_is_the_next_boundary() {
        assert_eq!(
            Resolution::Hourly.bucket_end(t("2026-08-12T14:00:00Z")),
            t("2026-08-12T15:00:00Z")
        );
        assert_eq!(
            Resolution::Daily.bucket_end(t("2026-08-12T14:00:00Z")),
            t("2026-08-13T00:00:00Z")
        );
        assert_eq!(
            Resolution::Weekly.bucket_end(t("2026-08-12T14:00:00Z")),
            t("2026-08-17T00:00:00Z")
        );
        assert_eq!(
            Resolution::Monthly.bucket_end(t("2026-08-12T14:00:00Z")),
            t("2026-09-01T00:00:00Z")
        );
    }

    #[test]
    fn test_bucket_end_rolls_the_year() {
        assert_eq!(
            Resolution::Monthly.bucket_end(t("2026-12-31T23:59:59Z")),
            t("2027-01-01T00:00:00Z")
        );
        assert_eq!(
            Resolution::Daily.bucket_end(t("2026-12-31T23:59:59Z")),
            t("2027-01-01T00:00:00Z")
        );
    }

    #[test]
    fn test_bucket_end_crosses_a_leap_day() {
        assert_eq!(
            Resolution::Daily.bucket_end(t("2028-02-28T12:00:00Z")),
            t("2028-02-29T00:00:00Z")
        );
        assert_eq!(
            Resolution::Monthly.bucket_end(t("2028-02-29T12:00:00Z")),
            t("2028-03-01T00:00:00Z")
        );
    }

    /// The window a statement was built with, read back off the bound values.
    fn window_of(
        resolution: Resolution,
        window: Window,
        now: DateTime<Utc>,
    ) -> (DateTime<Utc>, DateTime<Utc>) {
        let statement = refresh_statement(resolution, window, now);
        let values = statement
            .values
            .expect("a bounded window binds its start and end")
            .0;
        let bound = |v: &sea_orm::Value| match v {
            sea_orm::Value::ChronoDateTimeUtc(Some(b)) => **b,
            other => panic!("expected a timestamptz binding, got {other:?}"),
        };
        (bound(&values[0]), bound(&values[1]))
    }

    #[test]
    fn test_a_single_instant_still_covers_one_whole_bucket() {
        let instant = t("2026-08-12T14:22:00Z");
        for resolution in Resolution::ALL {
            let statement = refresh_statement(resolution, Window::Range(instant, instant), instant);
            assert!(statement.sql.contains(resolution.view()));
            let (start, end) = window_of(resolution, Window::Range(instant, instant), instant);
            assert_eq!(start, resolution.floor(instant));
            assert_eq!(end, resolution.bucket_end(instant));
            assert!(resolution.bucket_end(instant) > resolution.floor(instant));
        }
    }

    #[test]
    fn test_range_bounds_are_ordered_before_alignment() {
        let lo = t("2026-08-12T14:22:00Z");
        let hi = t("2026-08-14T09:00:00Z");
        let (start, end) = window_of(Resolution::Daily, Window::Range(hi, lo), hi);
        assert_eq!(start, t("2026-08-12T00:00:00Z"));
        assert_eq!(end, t("2026-08-15T00:00:00Z"));
    }

    #[test]
    fn test_since_a_future_instant_covers_that_instants_bucket() {
        let now = t("2026-08-12T14:22:00Z");
        let future = t("2026-09-20T10:00:00Z");
        let (start, end) = window_of(Resolution::Hourly, Window::Since(future), now);
        assert_eq!(start, t("2026-09-20T10:00:00Z"));
        assert_eq!(end, t("2026-09-20T11:00:00Z"));
    }

    #[test]
    fn test_since_covers_the_current_bucket() {
        let now = t("2026-08-12T14:22:00Z");
        let (start, end) = window_of(
            Resolution::Hourly,
            Window::Since(t("2026-08-12T14:05:00Z")),
            now,
        );
        assert_eq!(start, t("2026-08-12T14:00:00Z"));
        assert_eq!(end, t("2026-08-12T15:00:00Z"));
    }

    #[test]
    fn test_recent_reaches_each_views_lookback() {
        let now = t("2026-08-12T14:22:00Z");
        let (hourly_start, _) = window_of(Resolution::Hourly, Window::Recent, now);
        assert_eq!(hourly_start, t("2026-08-11T14:00:00Z"));
        let (daily_start, _) = window_of(Resolution::Daily, Window::Recent, now);
        assert_eq!(daily_start, t("2026-08-05T00:00:00Z"));
        let (weekly_start, _) = window_of(Resolution::Weekly, Window::Recent, now);
        assert_eq!(weekly_start, t("2026-07-27T00:00:00Z"));
        let (monthly_start, _) = window_of(Resolution::Monthly, Window::Recent, now);
        assert_eq!(monthly_start, t("2026-06-01T00:00:00Z"));
    }

    #[test]
    fn test_full_binds_no_values() {
        let statement = refresh_statement(Resolution::Hourly, Window::Full, Utc::now());
        assert!(statement.sql.contains("NULL, NULL"));
        assert!(statement.values.is_none());
    }

    #[test]
    fn test_touched_window_is_none_when_nothing_changed() {
        assert!(Window::touched(&TouchedRange::default()).is_none());
        let touched = TouchedRange {
            rows: 2,
            min_time: Some(t("2026-08-12T14:22:00Z")),
            max_time: Some(t("2026-08-12T16:10:00Z")),
        };
        let window = Window::touched(&touched).expect("a touched range implies a window");
        let (start, end) = window_of(Resolution::Hourly, window, t("2026-08-12T16:30:00Z"));
        assert_eq!(start, t("2026-08-12T14:00:00Z"));
        assert_eq!(end, t("2026-08-12T17:00:00Z"));
    }
}
