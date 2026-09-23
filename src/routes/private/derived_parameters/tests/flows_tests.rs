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
        r#""sp"."cadence" = 'high'"#,
        r#"COALESCE("sp"."is_active", TRUE) = TRUE"#,
        r#"JOIN "calculation_formulas" AS "d""#,
        r#""d"."tool_script_id" IS NOT NULL"#,
        r#"JOIN "calculation_formulas" AS "f""#,
        r#"JOIN "derived_parameter_sources" AS "dps""#,
        r#"NOT EXISTS(SELECT 1 FROM "readings" AS "r2""#,
        r#"ORDER BY "r"."site_id" ASC, "r"."time" ASC LIMIT 50000"#,
    ] {
        assert!(str::contains(&sql, expected), "{expected} missing: {sql}");
    }
}

/// Expected behaviour: the gap scan looks for instants the same way (Q230), so the sweep that
/// fills what a write missed does not mint a pulse at every visit a held source was measured at.
#[test]
fn test_the_gap_scan_counts_only_sources_read_at_the_instant() {
    let sql = sql(None);
    assert!(sql.contains(r#""dps"."alignment" = 'exact'"#), "{sql}");
}

#[test]
fn test_gap_fill_report_into_keeps_the_callers_entries() {
    use crate::routes::private::reprocessing_jobs::service::JobReport;
    let at: chrono::DateTime<chrono::Utc> = "2025-06-01T10:00:00Z".parse().unwrap();
    let gaps = super::GapFill {
        found: 5,
        filled: 4,
        refused_slots: 1,
        earliest_filled: Some(at),
        capped: false,
    };
    let report = gaps
        .report_into(JobReport::new().count("pruned", 3).scope("full_scan", true))
        .to_value();
    assert_eq!(
        report,
        serde_json::json!({
            "scope": {
                "full_scan": true,
                "capped_at_limit": false,
                "earliest_filled": "2025-06-01T10:00:00+00:00",
            },
            "counts": { "pruned": 3, "gaps_found": 5, "filled": 4, "refused_slots": 1 },
        })
    );
    // A pass that found nothing still reports its zeros, so a tick with no gaps reads as one.
    let empty = super::GapFill::default()
        .report_into(JobReport::new())
        .to_value();
    assert_eq!(empty["counts"]["gaps_found"], 0);
    assert!(empty["scope"].get("earliest_filled").is_none());
}
