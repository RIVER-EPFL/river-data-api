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
