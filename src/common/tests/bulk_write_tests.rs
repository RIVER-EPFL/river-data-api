use super::*;
use sea_orm::ExprTrait;

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

/// Scenario: a write reports the span it is about to touch.
/// Expected behaviour: the span is read from the caller's own query of the rows, aggregated over
/// it as a subquery, and the write itself carries no `RETURNING`: TimescaleDB holds every row a
/// hypertable `RETURNING` emits in executor memory (B392).
#[test]
fn test_the_span_query_aggregates_the_callers_rows_and_returns_nothing() {
    let mut update = Query::update();
    update
        .table(Alias::new("readings"))
        .value(
            Alias::new("site_id"),
            Expr::value(sea_orm::Value::Int(None)),
        )
        .and_where(Expr::col(Alias::new("stream_id")).eq(7));
    let rows = Query::select()
        .column(Alias::new("time"))
        .from(Alias::new("readings"))
        .and_where(Expr::col(Alias::new("stream_id")).eq(7))
        .to_owned();

    let spanned = Spanned::new(rows, update.to_owned());
    let (span_sql, _) = span_query(spanned.rows.clone()).build(PostgresQueryBuilder);
    assert!(span_sql.contains("MIN(\"touched\".\"time\")"), "{span_sql}");
    assert!(span_sql.contains("MAX(\"touched\".\"time\")"), "{span_sql}");
    assert!(
        span_sql.contains("FROM (SELECT \"time\" FROM \"readings\""),
        "{span_sql}"
    );
    assert!(span_sql.contains("AS \"touched\""), "{span_sql}");

    let written = spanned.write.build();
    assert!(written.sql.starts_with("UPDATE"), "{}", written.sql);
    assert!(!written.sql.contains("RETURNING"), "{}", written.sql);
}

/// A delete and an insert go through the same plain build, so neither can grow a `RETURNING`.
#[test]
fn test_a_delete_and_an_insert_are_built_plain_too() {
    let mut delete = Query::delete();
    delete
        .from_table(Alias::new("readings"))
        .and_where(Expr::col(Alias::new("stream_id")).eq(7));
    let built = Dml::Delete(delete.to_owned()).build();
    assert!(built.sql.starts_with("DELETE"), "{}", built.sql);
    assert!(!built.sql.contains("RETURNING"), "{}", built.sql);

    let mut insert = Query::insert();
    insert
        .into_table(Alias::new("readings"))
        .columns([Alias::new("stream_id")])
        .values_panic([7.into()]);
    let built = Dml::Insert(insert.to_owned()).build();
    assert!(built.sql.starts_with("INSERT"), "{}", built.sql);
    assert!(!built.sql.contains("RETURNING"), "{}", built.sql);
}
