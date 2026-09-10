use super::*;
use sea_orm::ExprTrait;

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

/// Scenario: a caller hands `mutation` a statement the builder produced instead of SQL text.
/// Expected behaviour: the wrapper it runs is the one `wrap_returning_time` writes by hand,
/// so a converted caller keeps reporting the same rows and span.
#[test]
fn test_summary_of_builds_the_wrapper_the_text_path_writes() {
    let mut update = Query::update();
    update
        .table(Alias::new("readings"))
        .value(
            Alias::new("site_id"),
            Expr::value(sea_orm::Value::Int(None)),
        )
        .and_where(Expr::col(Alias::new("stream_id")).eq(7));

    let built = summary_of(Dml::Update(update.to_owned()));

    assert!(
        built.sql.starts_with("WITH \"mutated\" AS (UPDATE"),
        "the CTE is named mutated: {}",
        built.sql
    );
    assert!(built.sql.contains("RETURNING \"time\""));
    assert!(built.sql.contains("COUNT(*)::bigint"));
    assert!(built.sql.contains("MIN(\"time\")"));
    assert!(built.sql.contains("MAX(\"time\")"));
    assert!(built.sql.contains("FROM \"mutated\""));
}

#[test]
fn test_summary_of_carries_a_delete_and_an_insert_the_same_way() {
    let mut delete = Query::delete();
    delete
        .from_table(Alias::new("readings"))
        .and_where(Expr::col(Alias::new("stream_id")).eq(7));
    let built = summary_of(Dml::Delete(delete.to_owned()));
    assert!(built.sql.starts_with("WITH \"mutated\" AS (DELETE"));
    assert!(built.sql.contains("RETURNING \"time\""));

    let mut insert = Query::insert();
    insert
        .into_table(Alias::new("readings"))
        .columns([Alias::new("stream_id")])
        .values_panic([7.into()]);
    let built = summary_of(Dml::Insert(insert.to_owned()));
    assert!(built.sql.starts_with("WITH \"mutated\" AS (INSERT"));
    assert!(built.sql.contains("RETURNING \"time\""));
}

/// Scenario: the built wrapper is handed to the executor.
///
/// Expected behaviour: it is a finished summary query, one `RETURNING` and one `mutated`, so
/// nothing downstream may wrap it a second time. Recognising it by its text does not work: the
/// builder quotes the CTE name and a hand-written wrapper does not.
#[test]
fn test_a_built_summary_is_already_the_whole_query() {
    let mut update = Query::update();
    update
        .table(Alias::new("readings"))
        .value(
            Alias::new("deployment_id"),
            Expr::value(sea_orm::Value::Int(None)),
        )
        .and_where(Expr::col(Alias::new("deployment_id")).eq(7));
    let built = summary_of(Dml::Update(update.to_owned()));

    assert_eq!(built.sql.matches("RETURNING").count(), 1, "{}", built.sql);
    assert!(
        built.sql.trim_end().ends_with("FROM \"mutated\""),
        "{}",
        built.sql
    );
    assert!(
        !built.sql.starts_with("WITH mutated AS"),
        "the builder quotes the CTE name, so a text sniff for the unquoted form misses it: {}",
        built.sql
    );
}
