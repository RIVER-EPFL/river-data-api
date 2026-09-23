use super::*;

/// Scenario: the retag rewrite is built rather than assembled per arm by `format!`.
/// Expected behaviour: a fixed target writes the value it was given, and 'declared' takes the
/// stream's own; both compare with IS DISTINCT FROM, because a NULL reads as continuous.
#[test]
fn test_retag_takes_a_fixed_target_or_the_stream_s_own() {
    let ids = vec![Uuid::nil()];

    let fixed = build_update(&retag_readings(Some("spot"), &ids, &ids, None));
    assert!(fixed.contains(r#"UPDATE "readings""#), "{fixed}");
    assert!(fixed.contains(r#"SET "measurement_type" ="#), "{fixed}");
    assert!(
        fixed.contains(r#""readings"."measurement_type" IS DISTINCT FROM 'spot'"#),
        "{fixed}"
    );
    assert!(
        !fixed.contains(r#"SET "measurement_type" = "data_streams""#),
        "a fixed target writes the value it was given: {fixed}"
    );

    let declared = build_update(&retag_readings(None, &ids, &ids, None));
    assert!(declared.contains(r#"FROM "data_streams""#), "{declared}");
    assert!(
        declared.contains(
            r#""readings"."measurement_type" IS DISTINCT FROM "data_streams"."measurement_type""#
        ),
        "{declared}"
    );
}

/// A reading ingested before attribution backfill carries no sensor_id and belongs to the sensor's
/// streams all the same, so the scope matches by ownership as well as by the reading's own column.
#[test]
fn test_retag_scope_matches_by_stream_ownership_too() {
    let ids = vec![Uuid::nil()];
    let sql = build_update(&retag_readings(Some("spot"), &ids, &ids, Some("cnet")));

    assert!(sql.contains(r#""readings"."sensor_id" IN"#), "{sql}");
    assert!(sql.contains(r#""readings"."stream_id" IN"#), "{sql}");
    assert_eq!(
        sql.matches(r#"SELECT "id" FROM "data_streams""#).count(),
        2,
        "one subquery for the sensor's streams, one for the source system's: {sql}"
    );
}

/// Scenario: the retag reads the span it is about to write before writing it (B392).
/// Expected behaviour: the query of the rows carries the same scope and the same IS DISTINCT FROM
/// comparison as the write, and joins `data_streams` only on the arm that reads it, so the two
/// cannot drift into naming different rows.
#[test]
fn test_the_span_query_selects_the_rows_the_retag_writes() {
    let ids = vec![Uuid::nil()];

    let (write, rows) = retag_readings(Some("spot"), &ids, &ids, Some("cnet")).as_sql();
    assert!(
        rows.starts_with(r#"SELECT "time" FROM "readings""#),
        "{rows}"
    );
    assert!(
        !rows.contains(r#"FROM "readings", "data_streams""#),
        "{rows}"
    );
    for predicate in [
        r#""readings"."measurement_type" IS DISTINCT FROM 'spot'"#,
        r#""readings"."sensor_id" IN"#,
        r#""readings"."stream_id" IN"#,
    ] {
        assert!(write.contains(predicate), "{write}");
        assert!(rows.contains(predicate), "{rows}");
    }

    let (_, declared) = retag_readings(None, &ids, &ids, None).as_sql();
    assert!(declared.contains(r#""data_streams""#), "{declared}");
    assert!(
        declared.contains(
            r#""readings"."measurement_type" IS DISTINCT FROM "data_streams"."measurement_type""#
        ),
        "{declared}"
    );
}

/// The source system is optional, and an absent one adds no arm rather than a NULL comparison.
#[test]
fn test_retag_scope_leaves_out_an_absent_source_system() {
    let ids = vec![Uuid::nil()];
    let sql = build_update(&retag_readings(Some("spot"), &ids, &ids, None));
    assert_eq!(
        sql.matches(r#"SELECT "id" FROM "data_streams""#).count(),
        1,
        "{sql}"
    );
}

/// Scenario: a retag records each reading it moves on the ledger (Q118), read before the write.
/// Expected behaviour: the ledger insert selects the same rows as the write, records the stored
/// classification as `old` and the one the write gives as `new`, and names the job.
#[test]
fn test_the_retag_ledger_records_the_rows_the_retag_writes() {
    let ids = vec![Uuid::nil()];
    let job = Uuid::from_u128(7);

    let fixed = retag_ledger(Some("spot"), &ids, &ids, Some("cnet"), Some(job))
        .to_string(sea_orm::sea_query::PostgresQueryBuilder);
    assert!(
        fixed.starts_with(r#"INSERT INTO "reading_decisions""#),
        "{fixed}"
    );
    assert!(fixed.contains("'retag'"), "{fixed}");
    assert!(fixed.contains(&job.to_string()), "{fixed}");
    assert!(
        fixed.contains(r#"jsonb_build_object('measurement_type', "readings"."measurement_type")"#),
        "{fixed}"
    );
    assert!(
        fixed.contains("jsonb_build_object('measurement_type', 'spot')"),
        "{fixed}"
    );
    let (write, _) = retag_readings(Some("spot"), &ids, &ids, Some("cnet")).as_sql();
    for predicate in [
        r#""readings"."measurement_type" IS DISTINCT FROM 'spot'"#,
        r#""readings"."sensor_id" IN"#,
        r#""readings"."stream_id" IN"#,
    ] {
        assert!(write.contains(predicate), "{write}");
        assert!(fixed.contains(predicate), "{fixed}");
    }

    let declared = retag_ledger(None, &ids, &ids, None, None)
        .to_string(sea_orm::sea_query::PostgresQueryBuilder);
    assert!(
        declared.contains(
            r#"jsonb_build_object('measurement_type', "data_streams"."measurement_type")"#
        ),
        "{declared}"
    );
    assert!(
        declared.contains(
            r#""readings"."measurement_type" IS DISTINCT FROM "data_streams"."measurement_type""#
        ),
        "{declared}"
    );
}

/// The write's own SQL. The span query beside it is asserted on separately.
fn build_update(spanned: &crate::common::bulk_write::Spanned) -> String {
    spanned.as_sql().0
}
