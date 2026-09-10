use super::*;

#[test]
fn test_wrap_returning_time_wraps_an_update() {
    let wrapped = wrap_returning_time("UPDATE readings SET site_id = NULL WHERE stream_id = $1");
    assert!(wrapped.starts_with(
        "WITH mutated AS (UPDATE readings SET site_id = NULL WHERE stream_id = $1 RETURNING time)"
    ));
    assert!(wrapped.contains("COUNT(*)::bigint AS touched_rows"));
    assert!(wrapped.contains("MIN(time) AS min_time"));
    assert!(wrapped.contains("MAX(time) AS max_time"));
}

#[test]
fn test_wrap_returning_time_strips_a_trailing_semicolon() {
    let wrapped = wrap_returning_time("DELETE FROM readings WHERE stream_id = $1 ;\n");
    assert!(wrapped.contains("WHERE stream_id = $1 RETURNING time)"));
    assert!(!wrapped.contains(';'));
}

#[test]
fn test_touched_range_span_needs_both_bounds() {
    let empty = TouchedRange::default();
    assert!(empty.is_empty());
    assert!(empty.span().is_none());

    let t = DateTime::parse_from_rfc3339("2026-08-12T14:22:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let one = TouchedRange {
        rows: 1,
        min_time: Some(t),
        max_time: Some(t),
    };
    assert!(!one.is_empty());
    assert_eq!(one.span(), Some((t, t)));
}

#[test]
fn test_merge_widens_the_span_and_sums_the_rows() {
    let early = DateTime::parse_from_rfc3339("2026-08-12T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let late = DateTime::parse_from_rfc3339("2026-08-12T18:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let a = TouchedRange {
        rows: 2,
        min_time: Some(late),
        max_time: Some(late),
    };
    let b = TouchedRange {
        rows: 3,
        min_time: Some(early),
        max_time: Some(early),
    };
    let merged = a.merge(b);
    assert_eq!(merged.rows, 5);
    assert_eq!(merged.span(), Some((early, late)));
}

#[test]
fn test_merge_with_an_empty_range_keeps_the_other_span() {
    let t = DateTime::parse_from_rfc3339("2026-08-12T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let one = TouchedRange {
        rows: 1,
        min_time: Some(t),
        max_time: Some(t),
    };
    assert_eq!(one.merge(TouchedRange::default()), one);
    assert_eq!(TouchedRange::default().merge(one), one);
    assert_eq!(
        TouchedRange::default().merge(TouchedRange::default()),
        TouchedRange::default()
    );
}
