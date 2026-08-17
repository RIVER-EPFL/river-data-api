//! One (site, parameter) series over a time window, fetched without going through HTTP.
//!
//! The two series handlers (`sites::readings::get_site_readings`, `sites::aggregates::
//! get_site_aggregates`) are multi-parameter reads that also carry severity joins, a sensor-split
//! dimension and format negotiation, all inline. This module is the narrow single-series read the
//! bot's plot commands need; it deliberately does not try to be their shared implementation, since
//! generalising it far enough would just reproduce those handlers with a worse shape.
//!
//! # Why the raw tier filters more than `/latest` does
//!
//! [`Tier::Raw`] applies the same predicates the continuous aggregates were built with (unflagged,
//! non-spot, first replicate). That is intentional and differs from `commands::latest`, which
//! filters only `replicate_index`. Within one chart the tier flips with the window width, so a
//! flagged spike visible on `/1d` (raw) and absent from `/30d` (hourly rollup) would read as a
//! rendering bug. `/latest` answers a different question ("what did the sensor last report")
//! and is right to keep its own filter.

use chrono::{DateTime, Duration, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, FromQueryResult, Statement};
use uuid::Uuid;

use super::aggregates::Resolution;

/// The single value projection every series read in this codebase uses: a replicate group serves
/// its sample mean, an uncorrected reading its raw value.
pub const VALUE_EXPR: &str = "COALESCE(smp.mean, r.calibrated_value, r.raw_value)";

/// The predicates that make a raw read agree with the continuous aggregates.
pub const CAGG_PARITY_FILTERS: &str = "r.replicate_index = 0 AND r.is_flagged IS NOT TRUE AND r.measurement_type IS DISTINCT FROM 'spot'";

/// Above this, a series is decimated before rendering. Roughly 4/3 of the widest plot area we
/// draw, so there is at least one point per pixel column and no more than two.
pub const MAX_POINTS: usize = 1400;

/// Which storage tier a series was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Individual readings.
    Raw,
    /// A continuous aggregate, pre-bucketed.
    Rollup(Resolution),
}

impl Tier {
    /// How this tier is described to a human, for a chart subtitle.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Tier::Raw => "raw readings",
            Tier::Rollup(Resolution::Hourly) => "hourly means",
            Tier::Rollup(Resolution::Daily) => "daily means",
            Tier::Rollup(Resolution::Weekly) => "weekly means",
            Tier::Rollup(Resolution::Monthly) => "monthly means",
        }
    }

    /// The gap width, in seconds, beyond which a chart should break the line rather than bridge
    /// it. Mirrors `GAP_THRESHOLDS` in the dashboard so both draw the same dropout.
    #[must_use]
    pub fn gap_seconds(self) -> i64 {
        match self {
            Tier::Raw => 1_800,
            Tier::Rollup(Resolution::Hourly) => 10_800,
            Tier::Rollup(Resolution::Daily) => 259_200,
            // A weekly or monthly series is too coarse for gap detection to mean anything.
            Tier::Rollup(_) => i64::MAX,
        }
    }
}

/// One point of a series. At [`Tier::Raw`] this is a reading; at a rollup it is a bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeriesPoint {
    pub time: DateTime<Utc>,
    /// Bucket mean at a rollup tier, the reading value at raw.
    pub value: f64,
    /// Bucket extrema. Both `None` at raw, where the point is its own extreme.
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Readings behind this point. Always 1 at raw.
    pub count: i64,
}

/// A series and the tier it came from.
#[derive(Debug, Clone)]
pub struct Series {
    pub tier: Tier,
    pub points: Vec<SeriesPoint>,
}

impl Series {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }
}

#[derive(Debug, FromQueryResult)]
struct RollupRow {
    bucket: DateTime<Utc>,
    avg_value: Option<f64>,
    min_value: Option<f64>,
    max_value: Option<f64>,
    count: Option<i64>,
}

#[derive(Debug, FromQueryResult)]
struct RawRow {
    time: DateTime<Utc>,
    value: Option<f64>,
}

/// The tier to read a `window`-wide series from.
///
/// Window width alone decides this, so a caller can name the tier before fetching. The 48-hour cut
/// is what keeps a 6-hour window off the hourly rollup, where it would resolve to six points.
#[must_use]
pub fn tier_for(window: Duration) -> Tier {
    let hours = window.num_hours();
    if hours <= 48 {
        Tier::Raw
    } else if hours <= 45 * 24 {
        Tier::Rollup(Resolution::Hourly)
    } else if hours <= 400 * 24 {
        Tier::Rollup(Resolution::Daily)
    } else if hours <= 3 * 365 * 24 {
        Tier::Rollup(Resolution::Weekly)
    } else {
        Tier::Rollup(Resolution::Monthly)
    }
}

/// Reduce `points` to at most `target`, keeping the extremes of each equal-width time bucket.
///
/// Raw cadence is a per-deployment property, so a window that is narrow in time can still be wide
/// in points. Min/max envelope decimation is used rather than every-nth or LTTB because a
/// single-sample spike or dropout has to survive: those are the features an operator is looking
/// for. First and last points are always preserved.
#[must_use]
pub fn decimate(points: Vec<SeriesPoint>, target: usize) -> Vec<SeriesPoint> {
    if target < 2 || points.len() <= target {
        return points;
    }
    let first_point = points[0];
    let last_point = points[points.len() - 1];
    let first = first_point.time.timestamp_millis();
    let last = last_point.time.timestamp_millis();
    let span = (last - first).max(1);
    // Two points survive per bucket (the min and the max), so aim for half as many buckets, less
    // the two the endpoint reinstatement below may add back.
    let buckets = (target.saturating_sub(2) / 2).max(1) as i64;

    let mut out: Vec<SeriesPoint> = Vec::with_capacity(target + 2);
    let mut bucket_lo: Option<SeriesPoint> = None;
    let mut bucket_hi: Option<SeriesPoint> = None;
    let mut current = 0i64;

    let flush = |lo: Option<SeriesPoint>, hi: Option<SeriesPoint>, out: &mut Vec<SeriesPoint>| {
        match (lo, hi) {
            (Some(l), Some(h)) if l.time == h.time => out.push(l),
            (Some(l), Some(h)) => {
                // Emit in time order so the line never doubles back on itself.
                if l.time <= h.time {
                    out.push(l);
                    out.push(h);
                } else {
                    out.push(h);
                    out.push(l);
                }
            }
            (Some(p), None) | (None, Some(p)) => out.push(p),
            (None, None) => {}
        }
    };

    for p in points {
        let idx = ((p.time.timestamp_millis() - first) * buckets / span).min(buckets - 1);
        if idx != current {
            flush(bucket_lo.take(), bucket_hi.take(), &mut out);
            current = idx;
        }
        match bucket_lo {
            Some(l) if l.value <= p.value => {}
            _ => bucket_lo = Some(p),
        }
        match bucket_hi {
            Some(h) if h.value >= p.value => {}
            _ => bucket_hi = Some(p),
        }
    }
    flush(bucket_lo, bucket_hi, &mut out);

    // A bucket contributes its extremes, and the true endpoints are usually not extreme, so they
    // have to be reinstated. Without this the chart's x-range silently shrinks and the most recent
    // reading (the one a `/plot` is usually asked for) is not the one drawn last.
    if out.first().is_none_or(|p| p.time != first_point.time) {
        out.insert(0, first_point);
    }
    if out.last().is_none_or(|p| p.time != last_point.time) {
        out.push(last_point);
    }
    out
}

/// Fetch one series over `[start, end]` from `tier`.
///
/// Points with a NULL value are dropped rather than zero-filled: an empty bucket is an absence,
/// and drawing it as zero would invent a reading. Ordered ascending by time.
pub async fn fetch_series(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    tier: Tier,
) -> Result<Series, DbErr> {
    let points = match tier {
        Tier::Rollup(resolution) => {
            // The count-weighted collapse of the sensor dimension, matching
            // `sites::aggregates::get_site_aggregates` so the bot and the dashboard agree
            // bucket-for-bucket.
            let sql = format!(
                r"
                SELECT
                    bucket,
                    CASE WHEN SUM(count) > 0 THEN SUM(sum_value) / SUM(count) ELSE NULL END AS avg_value,
                    MIN(min_value) AS min_value,
                    MAX(max_value) AS max_value,
                    SUM(count)::bigint AS count
                FROM {view}
                WHERE site_id = $1
                  AND parameter_id = $2
                  AND bucket >= $3
                  AND bucket <= $4
                GROUP BY bucket
                ORDER BY bucket ASC
                ",
                view = resolution.view(),
            );
            db.query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                &sql,
                [
                    site_id.into(),
                    parameter_id.into(),
                    start.into(),
                    end.into(),
                ],
            ))
            .await?
            .into_iter()
            .filter_map(|row| RollupRow::from_query_result(&row, "").ok())
            .filter_map(|r| {
                r.avg_value.map(|value| SeriesPoint {
                    time: r.bucket,
                    value,
                    min: r.min_value,
                    max: r.max_value,
                    count: r.count.unwrap_or(0),
                })
            })
            .collect()
        }
        Tier::Raw => {
            let sql = format!(
                r"
                SELECT r.time AS time, {VALUE_EXPR} AS value
                FROM readings r
                LEFT JOIN samples smp ON smp.id = r.sample_id
                WHERE r.site_id = $1
                  AND r.parameter_id = $2
                  AND r.time >= $3
                  AND r.time <= $4
                  AND {CAGG_PARITY_FILTERS}
                ORDER BY r.time ASC
                "
            );
            db.query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                &sql,
                [
                    site_id.into(),
                    parameter_id.into(),
                    start.into(),
                    end.into(),
                ],
            ))
            .await?
            .into_iter()
            .filter_map(|row| RawRow::from_query_result(&row, "").ok())
            .filter_map(|r| {
                r.value.map(|value| SeriesPoint {
                    time: r.time,
                    value,
                    min: None,
                    max: None,
                    count: 1,
                })
            })
            .collect()
        }
    };

    Ok(Series { tier, points })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(secs: i64, value: f64) -> SeriesPoint {
        SeriesPoint {
            time: DateTime::from_timestamp(secs, 0).unwrap(),
            value,
            min: None,
            max: None,
            count: 1,
        }
    }

    #[test]
    fn tier_ladder_boundaries() {
        assert_eq!(tier_for(Duration::hours(6)), Tier::Raw);
        assert_eq!(tier_for(Duration::hours(24)), Tier::Raw);
        // The 48h cut is what keeps a short window off the hourly rollup.
        assert_eq!(tier_for(Duration::hours(48)), Tier::Raw);
        assert_eq!(
            tier_for(Duration::hours(49)),
            Tier::Rollup(Resolution::Hourly)
        );
        assert_eq!(
            tier_for(Duration::days(7)),
            Tier::Rollup(Resolution::Hourly)
        );
        assert_eq!(
            tier_for(Duration::days(30)),
            Tier::Rollup(Resolution::Hourly)
        );
        assert_eq!(
            tier_for(Duration::days(45)),
            Tier::Rollup(Resolution::Hourly)
        );
        assert_eq!(
            tier_for(Duration::days(46)),
            Tier::Rollup(Resolution::Daily)
        );
        assert_eq!(
            tier_for(Duration::days(400)),
            Tier::Rollup(Resolution::Daily)
        );
        assert_eq!(
            tier_for(Duration::days(401)),
            Tier::Rollup(Resolution::Weekly)
        );
        assert_eq!(
            tier_for(Duration::days(4 * 365)),
            Tier::Rollup(Resolution::Monthly)
        );
    }

    #[test]
    fn a_one_day_window_reads_raw_like_the_legacy_bot() {
        assert_eq!(tier_for(Duration::days(1)), Tier::Raw);
    }

    #[test]
    fn decimate_leaves_short_series_alone() {
        let pts: Vec<_> = (0..50).map(|i| pt(i * 60, i as f64)).collect();
        let out = decimate(pts.clone(), 1400);
        assert_eq!(out, pts);
    }

    #[test]
    fn decimate_respects_the_target() {
        let pts: Vec<_> = (0..10_000).map(|i| pt(i * 60, (i % 97) as f64)).collect();
        let out = decimate(pts, 1400);
        assert!(out.len() <= 1400, "got {} points", out.len());
        assert!(out.len() > 100, "decimation should not collapse the series");
    }

    #[test]
    fn decimate_preserves_time_order() {
        let pts: Vec<_> = (0..5_000)
            .map(|i| pt(i * 60, ((i as f64) / 7.0).sin()))
            .collect();
        let out = decimate(pts, 400);
        assert!(
            out.windows(2).all(|w| w[0].time <= w[1].time),
            "decimated points must stay in time order"
        );
    }

    #[test]
    fn decimate_preserves_first_and_last() {
        let pts: Vec<_> = (0..5_000).map(|i| pt(i * 60, (i % 13) as f64)).collect();
        let first = pts[0];
        let last = pts[pts.len() - 1];
        let out = decimate(pts, 200);
        assert_eq!(out.first().copied(), Some(first));
        assert_eq!(out.last().copied(), Some(last));
    }

    #[test]
    fn decimate_keeps_a_single_sample_spike() {
        // Scenario: a flat series with one extreme reading, the shape an operator is looking for.
        // Expected behaviour: the spike survives decimation; every-nth sampling would lose it.
        let mut pts: Vec<_> = (0..10_000).map(|i| pt(i * 60, 10.0)).collect();
        pts[4_321] = pt(4_321 * 60, 999.0);
        let out = decimate(pts, 300);
        assert!(
            out.iter().any(|p| (p.value - 999.0).abs() < f64::EPSILON),
            "the spike must survive decimation"
        );
    }

    #[test]
    fn decimate_keeps_a_single_sample_dropout() {
        let mut pts: Vec<_> = (0..10_000).map(|i| pt(i * 60, 10.0)).collect();
        pts[777] = pt(777 * 60, -50.0);
        let out = decimate(pts, 300);
        assert!(
            out.iter().any(|p| (p.value + 50.0).abs() < f64::EPSILON),
            "the dropout must survive decimation"
        );
    }

    #[test]
    fn gap_thresholds_match_the_dashboard() {
        assert_eq!(Tier::Raw.gap_seconds(), 1_800);
        assert_eq!(Tier::Rollup(Resolution::Hourly).gap_seconds(), 10_800);
        assert_eq!(Tier::Rollup(Resolution::Daily).gap_seconds(), 259_200);
        assert_eq!(Tier::Rollup(Resolution::Weekly).gap_seconds(), i64::MAX);
    }
}
