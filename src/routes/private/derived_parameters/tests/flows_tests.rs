use super::gap_scan;
use sea_orm::sea_query::PostgresQueryBuilder;
use uuid::Uuid;

fn missed(tool: Uuid, site: Uuid, time: chrono::DateTime<chrono::Utc>) -> super::Gap {
    super::Gap { tool_script_id: tool, site_id: site, time, backfill: false }
}

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
        r#"SELECT "r"."site_id", "r"."time", "d"."tool_script_id", MAX("r"."ingested_at") AS "input_written", MAX("sp"."created_at") AS "slot_declared" FROM "readings" AS "r""#,
        r#"GROUP BY "r"."site_id", "r"."time", "d"."tool_script_id""#,
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
        by_calculation: std::collections::BTreeMap::new(),
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

/// Scenario: two calculations at one site, and one of them at a second site, each with a gap at
/// the same instant, one of which the recompute failed.
///
/// Expected behaviour: each calculation is credited with its own filled gaps at each site, never
/// its neighbour's, and a gap the recompute did not fill is credited to nobody.
#[test]
fn test_fills_by_calculation_credits_each_gap_to_its_own_calculation() {
    use std::collections::HashSet;
    let (pco2, doc, saxon, sion) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(10), Uuid::from_u128(11));
    let at: chrono::DateTime<chrono::Utc> = "2025-06-01T10:00:00Z".parse().unwrap();
    let later = at + chrono::Duration::hours(1);
    let gaps = [missed(pco2, saxon, at), missed(doc, saxon, at), missed(pco2, saxon, later), missed(pco2, sion, at)];
    let filled: HashSet<_> = [(saxon, at), (saxon, later)].into_iter().collect();

    let fills = super::fills_by_calculation(&gaps, &filled);

    assert_eq!(fills.len(), 2);
    assert_eq!(fills[&pco2].len(), 1);
    assert_eq!(fills[&pco2][&saxon].values, 2);
    assert_eq!(fills[&pco2][&saxon].instants, vec![at, later]);
    assert_eq!(fills[&doc][&saxon].values, 1);
    // Sion's recompute failed, so nothing was filled there.
    assert!(!fills[&pco2].contains_key(&sion));
}

#[test]
fn test_fills_by_calculation_lists_instants_up_to_the_cap_and_counts_them_all() {
    use std::collections::HashSet;
    let (pco2, saxon) = (Uuid::from_u128(1), Uuid::from_u128(10));
    let start: chrono::DateTime<chrono::Utc> = "2025-06-01T00:00:00Z".parse().unwrap();
    let gaps: Vec<_> = (0..super::FILL_INSTANTS_LISTED + 5)
        .map(|i| missed(pco2, saxon, start + chrono::Duration::minutes(i as i64)))
        .collect();
    let filled: HashSet<_> = gaps.iter().map(|g| (g.site_id, g.time)).collect();

    let fills = super::fills_by_calculation(&gaps, &filled);

    assert_eq!(fills[&pco2][&saxon].values, super::FILL_INSTANTS_LISTED + 5);
    assert_eq!(fills[&pco2][&saxon].instants.len(), super::FILL_INSTANTS_LISTED);
}

#[test]
fn test_gap_fill_report_names_what_each_calculation_had_filled() {
    use crate::routes::private::reprocessing_jobs::service::JobReport;
    let (pco2, saxon) = (Uuid::from_u128(1), Uuid::from_u128(10));
    let at: chrono::DateTime<chrono::Utc> = "2025-06-01T10:00:00Z".parse().unwrap();
    let mut gaps = super::GapFill { found: 1, filled: 1, ..Default::default() };
    gaps.by_calculation
        .entry(pco2)
        .or_default()
        .insert(saxon, super::SiteFills { values: 1, backfilled: 2, instants: vec![at] });
    let report = gaps.report_into(JobReport::new()).to_value();
    assert_eq!(
        report["scope"]["filled_by_calculation"],
        serde_json::json!({
            pco2.to_string(): { saxon.to_string(): { "values": 1, "backfilled": 2, "instants": ["2025-06-01T10:00:00Z"] } }
        })
    );
}

#[test]
fn test_is_backfill_when_every_input_arrived_before_the_slot_was_declared() {
    let declared: chrono::DateTime<chrono::Utc> = "2025-06-01T10:00:00Z".parse().unwrap();
    let before = declared - chrono::Duration::days(30);
    let after = declared + chrono::Duration::minutes(5);
    assert!(super::is_backfill(Some(before), Some(declared)));
    assert!(!super::is_backfill(Some(after), Some(declared)));
    // A value that predates arrival tracking is history the slot was declared over.
    assert!(super::is_backfill(None, Some(declared)));
    // A slot that predates tracking cannot vouch for the value, so the fill is shown.
    assert!(!super::is_backfill(Some(before), None));
}

/// Expected behaviour: a backfill is counted apart and raises no instant, so the health row
/// counts only the fills a missed recompute left.
#[test]
fn test_fills_by_calculation_counts_a_backfill_apart() {
    use std::collections::HashSet;
    let (pco2, saxon) = (Uuid::from_u128(1), Uuid::from_u128(10));
    let at: chrono::DateTime<chrono::Utc> = "2025-06-01T10:00:00Z".parse().unwrap();
    let earlier = at - chrono::Duration::days(1);
    let gaps = [
        super::Gap { backfill: true, ..missed(pco2, saxon, earlier) },
        missed(pco2, saxon, at),
    ];
    let filled: HashSet<_> = [(saxon, earlier), (saxon, at)].into_iter().collect();

    let fills = super::fills_by_calculation(&gaps, &filled);

    assert_eq!(fills[&pco2][&saxon], super::SiteFills { values: 1, backfilled: 1, instants: vec![at] });
}
