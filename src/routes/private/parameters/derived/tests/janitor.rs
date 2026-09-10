use super::gap_scan;
use sea_orm::sea_query::PostgresQueryBuilder;

fn sql(since: Option<chrono::DateTime<chrono::Utc>>) -> String {
    gap_scan(since).to_string(PostgresQueryBuilder)
}

#[test]
fn test_gap_scan_bounds_the_readings_side_when_given_a_window() {
    let bounded = sql(Some(chrono::Utc::now()));
    assert!(
        str::contains(&bounded, r#""r"."time" >= "#),
        "a bounded run must not hash the whole hypertable: {bounded}"
    );

    let full = sql(None);
    assert!(
        !str::contains(&full, r#""r"."time" >= "#),
        "the periodic full run is the one that covers older drift: {full}"
    );
}

/// Expected behaviour: a source reading with no derived reading at the same slot and instant, over
/// the tool-entry slots of an active calculation, newest bound aside.
#[test]
fn test_gap_scan_is_the_anti_join_over_active_tool_slots() {
    let sql = sql(None);
    for expected in [
        r#"SELECT DISTINCT "r"."site_id", "r"."time" FROM "readings" AS "r""#,
        r#"JOIN "site_parameters" AS "sp""#,
        r#""sp"."entry_mode" = 'tool'"#,
        r#"COALESCE("sp"."is_active", TRUE) = TRUE"#,
        r#"JOIN "calculation_formulas" AS "d""#,
        r#"JOIN "derived_parameter_sources" AS "dps""#,
        r#"NOT EXISTS(SELECT 1 FROM "readings" AS "r2""#,
        r#"ORDER BY "r"."site_id" ASC, "r"."time" ASC LIMIT 50000"#,
    ] {
        assert!(str::contains(&sql, expected), "{expected} missing: {sql}");
    }
}
