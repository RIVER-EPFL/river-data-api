use super::*;

fn t(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

#[test]
fn test_floor_hourly_on_the_bucket_edge_is_the_edge() {
    assert_eq!(
        Resolution::Hourly.floor(t("2026-08-12T14:00:00Z")).unwrap(),
        t("2026-08-12T14:00:00Z")
    );
}

#[test]
fn test_floor_hourly_one_microsecond_after_the_edge() {
    assert_eq!(
        Resolution::Hourly
            .floor(t("2026-08-12T14:00:00.000001Z"))
            .unwrap(),
        t("2026-08-12T14:00:00Z")
    );
}

#[test]
fn test_floor_hourly_mid_bucket() {
    assert_eq!(
        Resolution::Hourly
            .floor(t("2026-08-12T14:22:33.456789Z"))
            .unwrap(),
        t("2026-08-12T14:00:00Z")
    );
}

#[test]
fn test_floor_daily_is_utc_midnight() {
    assert_eq!(
        Resolution::Daily.floor(t("2026-08-12T00:00:00Z")).unwrap(),
        t("2026-08-12T00:00:00Z")
    );
    assert_eq!(
        Resolution::Daily
            .floor(t("2026-08-12T00:00:00.000001Z"))
            .unwrap(),
        t("2026-08-12T00:00:00Z")
    );
    assert_eq!(
        Resolution::Daily
            .floor(t("2026-08-12T23:59:59.999999Z"))
            .unwrap(),
        t("2026-08-12T00:00:00Z")
    );
}

#[test]
fn test_floor_weekly_is_the_monday() {
    // 2026-08-12 is a Wednesday; the week bucket starts Monday 2026-08-10.
    assert_eq!(
        Resolution::Weekly.floor(t("2026-08-12T14:22:00Z")).unwrap(),
        t("2026-08-10T00:00:00Z")
    );
    assert_eq!(
        Resolution::Weekly.floor(t("2026-08-10T00:00:00Z")).unwrap(),
        t("2026-08-10T00:00:00Z")
    );
    assert_eq!(
        Resolution::Weekly
            .floor(t("2026-08-10T00:00:00.000001Z"))
            .unwrap(),
        t("2026-08-10T00:00:00Z")
    );
    // A Sunday belongs to the week that started six days earlier.
    assert_eq!(
        Resolution::Weekly.floor(t("2026-08-16T23:00:00Z")).unwrap(),
        t("2026-08-10T00:00:00Z")
    );
}

#[test]
fn test_floor_monthly_is_the_first() {
    assert_eq!(
        Resolution::Monthly
            .floor(t("2026-08-12T14:22:00Z"))
            .unwrap(),
        t("2026-08-01T00:00:00Z")
    );
    assert_eq!(
        Resolution::Monthly
            .floor(t("2026-08-01T00:00:00Z"))
            .unwrap(),
        t("2026-08-01T00:00:00Z")
    );
    assert_eq!(
        Resolution::Monthly
            .floor(t("2026-08-01T00:00:00.000001Z"))
            .unwrap(),
        t("2026-08-01T00:00:00Z")
    );
}

#[test]
fn test_floor_before_the_epoch() {
    assert_eq!(
        Resolution::Daily.floor(t("1969-12-30T05:00:00Z")).unwrap(),
        t("1969-12-30T00:00:00Z")
    );
    // 1969-12-29 is a Monday.
    assert_eq!(
        Resolution::Weekly.floor(t("1969-12-30T05:00:00Z")).unwrap(),
        t("1969-12-29T00:00:00Z")
    );
}

#[test]
fn test_bucket_end_is_the_next_boundary() {
    assert_eq!(
        Resolution::Hourly
            .bucket_end(t("2026-08-12T14:00:00Z"))
            .unwrap(),
        t("2026-08-12T15:00:00Z")
    );
    assert_eq!(
        Resolution::Daily
            .bucket_end(t("2026-08-12T14:00:00Z"))
            .unwrap(),
        t("2026-08-13T00:00:00Z")
    );
    assert_eq!(
        Resolution::Weekly
            .bucket_end(t("2026-08-12T14:00:00Z"))
            .unwrap(),
        t("2026-08-17T00:00:00Z")
    );
    assert_eq!(
        Resolution::Monthly
            .bucket_end(t("2026-08-12T14:00:00Z"))
            .unwrap(),
        t("2026-09-01T00:00:00Z")
    );
}

#[test]
fn test_bucket_end_rolls_the_year() {
    assert_eq!(
        Resolution::Monthly
            .bucket_end(t("2026-12-31T23:59:59Z"))
            .unwrap(),
        t("2027-01-01T00:00:00Z")
    );
    assert_eq!(
        Resolution::Daily
            .bucket_end(t("2026-12-31T23:59:59Z"))
            .unwrap(),
        t("2027-01-01T00:00:00Z")
    );
}

#[test]
fn test_bucket_end_crosses_a_leap_day() {
    assert_eq!(
        Resolution::Daily
            .bucket_end(t("2028-02-28T12:00:00Z"))
            .unwrap(),
        t("2028-02-29T00:00:00Z")
    );
    assert_eq!(
        Resolution::Monthly
            .bucket_end(t("2028-02-29T12:00:00Z"))
            .unwrap(),
        t("2028-03-01T00:00:00Z")
    );
}

/// The window a statement was built with, read back off the bound values.
fn window_of(
    resolution: Resolution,
    window: Window,
    now: DateTime<Utc>,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let statement = refresh_statement(resolution, window, now).unwrap();
    let values = statement
        .values
        .expect("a bounded window binds its start and end")
        .0;
    let bound = |v: &sea_orm::Value| match v {
        sea_orm::Value::ChronoDateTimeUtc(Some(b)) => *b,
        other => panic!("expected a timestamptz binding, got {other:?}"),
    };
    (bound(&values[0]), bound(&values[1]))
}

#[test]
fn test_a_single_instant_still_covers_one_whole_bucket() {
    let instant = t("2026-08-12T14:22:00Z");
    for resolution in Resolution::ALL {
        let statement =
            refresh_statement(resolution, Window::Range(instant, instant), instant).unwrap();
        assert!(statement.sql.contains(resolution.view()));
        let (start, end) = window_of(resolution, Window::Range(instant, instant), instant);
        assert_eq!(start, resolution.floor(instant).unwrap());
        assert_eq!(end, resolution.bucket_end(instant).unwrap());
        assert!(resolution.bucket_end(instant).unwrap() > resolution.floor(instant).unwrap());
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
    let statement = refresh_statement(Resolution::Hourly, Window::Full, Utc::now()).unwrap();
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
