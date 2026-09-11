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

fn build_update(update: &sea_query::UpdateStatement) -> String {
    update.to_string(sea_query::PostgresQueryBuilder)
}
