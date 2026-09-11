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
            sql.contains(r#"JOIN "calculation_formulas" AS "d""#),
            "{sql}"
        );
        assert!(
            sql.contains(r#"JOIN "derived_parameter_sources" AS "dps""#),
            "{sql}"
        );
        assert!(sql.contains(r#""sp"."entry_mode" = 'tool'"#), "{sql}");
        assert!(
            sql.contains(r#""sp"."parameter_id" = "d"."output_parameter_id""#),
            "{sql}"
        );
        assert!(
            sql.contains(r#"ORDER BY "r"."site_id" ASC, "r"."time" ASC"#),
            "{sql}"
        );
    }
}
