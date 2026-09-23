use super::*;

/// Scenario: the two derived-recompute scopes share one join shape.
/// Expected behaviour: both reach the tool-entered slot for the calculation's output, and each
/// orders by the instant, because the caller walks the rows in time order.
#[test]
fn test_derived_instants_reaches_the_tool_entered_slot_in_both_scopes() {
    for definition in [Some(Uuid::nil()), None] {
        let sql = derived_instants(sea_query::Condition::all(), definition)
            .to_string(sea_query::PostgresQueryBuilder);
        assert!(sql.contains(r#"SELECT DISTINCT"#), "{sql}");
        assert!(sql.contains(r#"FROM "readings" AS "r""#), "{sql}");
        assert!(
            sql.contains(r#"JOIN "calculation_formulas" AS "f""#),
            "{sql}"
        );
        assert!(
            sql.contains(r#"JOIN "calculation_formulas" AS "o""#),
            "{sql}"
        );
        assert!(
            sql.contains(r#"JOIN "derived_parameter_sources" AS "dps""#),
            "{sql}"
        );
        assert!(sql.contains(r#""sp"."entry_mode" = 'tool'"#), "{sql}");
        assert!(sql.contains(r#""sp"."cadence" = 'high'"#), "{sql}");
        assert!(
            sql.contains(r#""sp"."parameter_id" = "o"."output_parameter_id""#),
            "{sql}"
        );
        assert!(
            sql.contains(r#"ORDER BY "r"."site_id" ASC, "r"."time" ASC"#),
            "{sql}"
        );
    }
}

/// Scenario: a calculation reads one source from a stream and one held from the last visit (Q230).
///
/// Expected behaviour: only the source read at the instant makes an instant one the set computes
/// at. A held source stands between visits, so counting its own instants would put a pulse at
/// every visit the lab recorded, off the stream's grid and holding the pulse's own numbers.
#[test]
fn test_only_a_source_read_at_the_instant_makes_one_to_compute_at() {
    for definition in [Some(Uuid::nil()), None] {
        let sql = derived_instants(sea_query::Condition::all(), definition)
            .to_string(sea_query::PostgresQueryBuilder);
        assert!(sql.contains(r#""dps"."alignment" = 'exact'"#), "{sql}");
    }
}

/// Expected behaviour: the same holds for the instants a fresh assignment computes over.
#[test]
fn test_an_assignment_computes_over_the_instants_read_at() {
    let sql = instants_a_calculation_reads(Uuid::nil(), Uuid::nil())
        .to_string(sea_query::PostgresQueryBuilder);
    assert!(sql.contains(r#""dps"."alignment" = 'exact'"#), "{sql}");
}

/// Scenario: a parameter some calculation holds (Q230) is written or curated at a visit.
///
/// Expected behaviour: the instants to recompute are that calculation's own stream instants at the
/// site, from the changed instant up to the next live measurement of the held parameter, because
/// every pulse in between reads the changed value. A calculation that holds none of the
/// parameters is not in scope.
#[test]
fn test_a_held_source_reaches_the_pulses_until_its_next_measurement() {
    let at = chrono::DateTime::parse_from_rfc3339("2025-06-02T09:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let sql = held_instants(&[Uuid::nil()], &[Uuid::nil()], at, at)
        .to_string(sea_query::PostgresQueryBuilder);
    assert!(
        sql.contains(r#""dps"."alignment" = 'exact'"#),
        "a pulse is an instant of the stream the set reads: {sql}"
    );
    assert!(
        sql.contains(r#""hs"."alignment" = 'hold'"#),
        "only a calculation holding the parameter: {sql}"
    );
    assert!(
        sql.contains(r#""hf"."tool_script_id" = "f"."tool_script_id""#),
        "the next measurement is of the parameter this calculation holds: {sql}"
    );
    assert!(
        sql.contains(r#""r"."time" >= '2025-06-02 09:00:00.000000 +00:00'"#),
        "from the changed instant: {sql}"
    );
    assert!(
        sql.contains(r#""h"."time" > '2025-06-02 09:00:00.000000 +00:00'"#),
        "up to the next measurement after it: {sql}"
    );
    assert!(
        sql.contains(r#""h"."withdrawn_at" IS NULL"#),
        "a withdrawn row is no measurement to stop at: {sql}"
    );
    assert!(
        sql.contains(r#""h"."is_flagged" <> TRUE OR "h"."is_flagged" IS NULL"#),
        "nor is a flagged one: {sql}"
    );
}

/// Expected behaviour: a window recompute runs both arms, the instants the written parameter is
/// read at and the pulses that hold it.
#[test]
fn test_a_window_recompute_covers_the_held_pulses() {
    let params = serde_json::json!({
        "site_ids": [Uuid::nil().to_string()],
        "parameter_ids": [Uuid::nil().to_string()],
        "start": "2025-06-02T09:00:00Z",
        "end": "2025-06-02T09:00:00Z",
    });
    let statements = derived_recompute_instants(&params).expect("a window builds");
    assert_eq!(statements.len(), 2, "the exact arm and the held arm");
    // Bound, `IS NOT $n` is a syntax error: a comparison takes a parameter, `IS` does not.
    assert!(
        !statements[1].sql.contains("IS NOT $"),
        "{}",
        statements[1].sql
    );
    assert!(
        statements[1].sql.contains(r#""hs"."alignment""#),
        "{}",
        statements[1].sql
    );
}
