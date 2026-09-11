use super::*;
use chrono::Duration;

fn t(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

#[test]
fn advances_one_interval_when_on_time() {
    let next = next_run_after(
        t("2026-06-19T12:00:00Z"),
        Duration::hours(1),
        t("2026-06-19T12:00:01Z"),
    );
    assert_eq!(next, t("2026-06-19T13:00:00Z"));
}

#[test]
fn stays_drift_free_when_a_run_finishes_late() {
    // Fired at 12:00:30 instead of 12:00:00, next is still 13:00:00, not 13:00:30.
    let next = next_run_after(
        t("2026-06-19T12:00:00Z"),
        Duration::hours(1),
        t("2026-06-19T12:00:30Z"),
    );
    assert_eq!(next, t("2026-06-19T13:00:00Z"));
}

#[test]
fn snaps_forward_after_gap_discarding_backlog() {
    // Scheduler was down; 13:00, 14:00, 15:00 were all missed.
    let next = next_run_after(
        t("2026-06-19T12:00:00Z"),
        Duration::hours(1),
        t("2026-06-19T15:30:00Z"),
    );
    assert_eq!(next, t("2026-06-19T16:00:00Z"));
}

#[test]
fn boundary_exactly_on_grid_advances_strictly_past() {
    let next = next_run_after(
        t("2026-06-19T12:00:00Z"),
        Duration::hours(1),
        t("2026-06-19T13:00:00Z"),
    );
    assert_eq!(next, t("2026-06-19T14:00:00Z"));
}

#[test]
fn edit_applies_immediately_from_now() {
    let s = Schedule::every_secs(300);
    assert_eq!(
        s.next_after_edit(t("2026-06-19T12:00:30Z")),
        t("2026-06-19T12:05:30Z")
    );
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

/// The claim is what keeps two replicas off one slot, so the query it emits is asserted rather than
/// described: enabled rows only, due only, soonest first, and one row locked and skipped by a peer.
#[test]
fn due_schedules_claims_one_due_enabled_row_and_skips_a_peers() {
    use sea_orm::QueryTrait;
    let sql = due_schedules()
        .into_query()
        .to_string(sea_orm::sea_query::PostgresQueryBuilder);
    assert!(sql.contains(r#""schedules"."enabled" = TRUE"#), "{sql}");
    assert!(
        sql.contains(r#""schedules"."next_run_at" IS NOT NULL"#),
        "{sql}"
    );
    assert!(
        sql.contains(r#""next_run_at" <= CURRENT_TIMESTAMP"#),
        "{sql}"
    );
    assert!(
        sql.contains(r#"ORDER BY "schedules"."next_run_at" ASC"#),
        "{sql}"
    );
    assert!(sql.contains("LIMIT 1"), "{sql}");
    assert!(sql.contains("FOR UPDATE SKIP LOCKED"), "{sql}");
}
