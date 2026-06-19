//! Recurring-Service scheduling types and the pure cadence math (ADR 0001).
//!
//! Migration-independent on purpose: the `schedules` table, the per-replica scheduler tick that
//! reads it, and the SeaORM model land with the worker-pool migration. This module defines the
//! policy types and the drift-free next-run / catch-up decisions those pieces apply, so the tricky
//! timing logic is unit-tested in isolation (no DB) before it drives anything.

use chrono::{DateTime, Duration, Utc};

/// What to do when a schedule is due but the previous run of the same job is still active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlapPolicy {
    /// Don't enqueue while a run is still in flight (the safe default for janitor/sweeper).
    #[default]
    SkipIfRunning,
    /// Always enqueue, even if one is running — only for genuinely concurrency-safe jobs.
    AllowConcurrent,
}

impl OverlapPolicy {
    /// The stable string persisted to `schedules.overlap_policy`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SkipIfRunning => "skip_if_running",
            Self::AllowConcurrent => "allow_concurrent",
        }
    }

    /// Parse the persisted string, falling back to the default for an unknown/missing value so a
    /// hand-edited row can never make the scheduler panic.
    #[must_use]
    pub fn from_str_or_default(s: Option<&str>) -> Self {
        match s {
            Some("allow_concurrent") => Self::AllowConcurrent,
            _ => Self::SkipIfRunning,
        }
    }
}

/// What to do about runs missed while the scheduler was down (a deploy/restart gap).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CatchupPolicy {
    /// Fire a single run now to resync, then drop the missed backlog. Never replay every tick.
    #[default]
    RunOnce,
    /// Skip entirely; wait for the next scheduled slot.
    Skip,
}

impl CatchupPolicy {
    /// The stable string persisted to `schedules.catchup_policy`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RunOnce => "run_once",
            Self::Skip => "skip",
        }
    }

    /// Parse the persisted string, falling back to the default for an unknown/missing value.
    #[must_use]
    pub fn from_str_or_default(s: Option<&str>) -> Self {
        match s {
            Some("skip") => Self::Skip,
            _ => Self::RunOnce,
        }
    }
}

/// A recurring Service's cadence + policies. The persisted form (a `schedules` row) carries these
/// plus identity / enabled / tunables; this is the in-memory shape the scheduler reasons over and
/// that `Job::default_schedule` returns when seeding a schedule on first start.
#[derive(Debug, Clone)]
pub struct Schedule {
    pub interval: Duration,
    pub overlap: OverlapPolicy,
    pub catchup: CatchupPolicy,
}

impl Schedule {
    /// A schedule firing every `secs` seconds with the default skip-if-running / run-once policies.
    #[must_use]
    pub fn every_secs(secs: i64) -> Self {
        Self {
            interval: Duration::seconds(secs),
            overlap: OverlapPolicy::default(),
            catchup: CatchupPolicy::default(),
        }
    }

    #[must_use]
    pub fn with_overlap(mut self, overlap: OverlapPolicy) -> Self {
        self.overlap = overlap;
        self
    }

    #[must_use]
    pub fn with_catchup(mut self, catchup: CatchupPolicy) -> Self {
        self.catchup = catchup;
        self
    }

    /// Next run strictly after `now`, on the grid anchored at `anchor` (the schedule's current
    /// `next_run_at`), so cadence never drifts by a run's own duration. After a downtime gap the
    /// grid snaps forward to the first future slot, discarding the missed backlog (whether to *also*
    /// fire once now is the scheduler's `CatchupPolicy` decision).
    #[must_use]
    pub fn next_run_after(&self, anchor: DateTime<Utc>, now: DateTime<Utc>) -> DateTime<Utc> {
        next_run_after(anchor, self.interval, now)
    }

    /// New `next_run_at` to apply immediately when an operator edits the cadence, so a lowered
    /// interval takes effect now instead of waiting out the old one.
    #[must_use]
    pub fn next_after_edit(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now + self.interval
    }
}

/// Drift-free next grid point strictly after `now`, where the grid is `anchor + k * interval`
/// (`k >= 1`). A zero/negative interval is guarded to one second so a misconfigured schedule can't
/// busy-loop or stall.
#[must_use]
pub fn next_run_after(
    anchor: DateTime<Utc>,
    interval: Duration,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    let interval_ms = interval.num_milliseconds();
    if interval_ms <= 0 {
        return now + Duration::seconds(1);
    }
    let delta_ms = (now - anchor).num_milliseconds();
    let steps = if delta_ms < 0 { 0 } else { delta_ms / interval_ms };
    let mut next = anchor + interval * ((steps + 1) as i32);
    // Guard against rounding leaving us on/under `now`.
    while next <= now {
        next += interval;
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn advances_one_interval_when_on_time() {
        let next = next_run_after(t("2026-06-19T12:00:00Z"), Duration::hours(1), t("2026-06-19T12:00:01Z"));
        assert_eq!(next, t("2026-06-19T13:00:00Z"));
    }

    #[test]
    fn stays_drift_free_when_a_run_finishes_late() {
        // Fired at 12:00:30 instead of 12:00:00 — next is still 13:00:00, not 13:00:30.
        let next = next_run_after(t("2026-06-19T12:00:00Z"), Duration::hours(1), t("2026-06-19T12:00:30Z"));
        assert_eq!(next, t("2026-06-19T13:00:00Z"));
    }

    #[test]
    fn snaps_forward_after_gap_discarding_backlog() {
        // Scheduler was down; 13:00, 14:00, 15:00 were all missed.
        let next = next_run_after(t("2026-06-19T12:00:00Z"), Duration::hours(1), t("2026-06-19T15:30:00Z"));
        assert_eq!(next, t("2026-06-19T16:00:00Z"));
    }

    #[test]
    fn boundary_exactly_on_grid_advances_strictly_past() {
        let next = next_run_after(t("2026-06-19T12:00:00Z"), Duration::hours(1), t("2026-06-19T13:00:00Z"));
        assert_eq!(next, t("2026-06-19T14:00:00Z"));
    }

    #[test]
    fn edit_applies_immediately_from_now() {
        let s = Schedule::every_secs(300);
        assert_eq!(s.next_after_edit(t("2026-06-19T12:00:30Z")), t("2026-06-19T12:05:30Z"));
    }

    #[test]
    fn zero_interval_is_guarded() {
        let now = t("2026-06-19T12:00:00Z");
        assert!(next_run_after(now, Duration::zero(), now) > now);
    }

    #[test]
    fn defaults_are_skip_if_running_and_run_once() {
        let s = Schedule::every_secs(3600);
        assert_eq!(s.overlap, OverlapPolicy::SkipIfRunning);
        assert_eq!(s.catchup, CatchupPolicy::RunOnce);
    }
}
