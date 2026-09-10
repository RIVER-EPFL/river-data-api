//! Scenario: the three queries that decide what alarm evaluation sees.
//!
//! Expected behaviour: the continuous arm keeps a flagged reading, because an out-of-range value
//! should keep alerting after someone flags it; the spot arm drops flagged replicates, so a fully
//! flagged group is evaluated at nothing. The continuous aggregates exclude flagged rows and the
//! curated serving arms do too, which is what makes the continuous arm here easy to "correct".

use super::*;
use crate::common::served::{CONTINUOUS_ROWS, SERVED_SPOT};

/// Every arm of every alarm query, paired with whether it is the spot arm.
fn arms() -> Vec<(&'static str, String, bool)> {
    let site = Uuid::nil();
    vec![
        (
            "violations_query continuous",
            violations_query(site, None, 1)
                .to_string(PostgresQueryBuilder)
                .split("UNION ALL")
                .next()
                .expect("continuous arm")
                .to_string(),
            false,
        ),
        (
            "violations_query spot",
            violations_query(site, None, 1)
                .to_string(PostgresQueryBuilder)
                .split("UNION ALL")
                .nth(1)
                .expect("spot arm")
                .to_string(),
            true,
        ),
        (
            "latest_served_query continuous",
            latest_served_query(false, "$1", "$2").to_string(PostgresQueryBuilder),
            false,
        ),
        (
            "latest_served_query spot",
            latest_served_query(true, "$1", "$2").to_string(PostgresQueryBuilder),
            true,
        ),
        (
            "ordered_query continuous",
            ordered_query(false).to_string(PostgresQueryBuilder),
            false,
        ),
        (
            "ordered_query spot",
            ordered_query(true).to_string(PostgresQueryBuilder),
            true,
        ),
    ]
}

/// Whether `sql` carries every clause of `rule`, however it is spelled. A built arm quotes its
/// identifiers and writes `NOT <col>` where a spelled one writes `IS NOT TRUE`; the two agree
/// because `unverified` is `NOT NULL`.
fn carries(sql: &str, rule: &str) -> bool {
    let unquoted = sql.replace('"', "");
    rule.split(" AND ").all(|clause| {
        let clause = clause.trim();
        let built = clause.replace("r.unverified IS NOT TRUE", "NOT r.unverified");
        unquoted.contains(clause) || unquoted.contains(&built)
    })
}

#[test]
fn test_the_continuous_alarm_arms_keep_a_flagged_reading() {
    for (name, sql, spot) in arms() {
        if spot {
            continue;
        }
        assert!(
            !sql.contains("is_flagged"),
            "{name} filters flagged readings out of alarm evaluation: {sql}"
        );
        assert!(
            carries(&sql, CONTINUOUS_ROWS),
            "{name} does not serve the continuous cadence: {sql}"
        );
    }
}

#[test]
fn test_the_spot_alarm_arms_evaluate_only_live_replicates() {
    for (name, sql, spot) in arms() {
        if !spot {
            continue;
        }
        assert!(
            carries(&sql, SERVED_SPOT),
            "{name} evaluates replicates a curated surface would not serve: {sql}"
        );
        assert!(
            sql.contains("smp.mean"),
            "{name} does not evaluate the instant at its sample mean: {sql}"
        );
    }
}

/// Scenario: the thresholds table shows one current value per slot.
/// Expected behaviour: the latest-value CTE picks one row per slot, prefers a continuous reading
/// over a spot one at the same instant, reads the sample mean where there is one, and stays inside
/// the recent chunks.
#[test]
fn test_the_latest_slot_value_is_one_recent_unflagged_row_per_slot() {
    let sql = super::latest_slot_values_sql();
    assert!(
        sql.contains(r#"DISTINCT ON ("r"."site_id", "r"."parameter_id")"#),
        "one row per slot: {sql}"
    );
    assert!(
        sql.contains("COALESCE(smp.mean, r.calibrated_value, r.raw_value)"),
        "the sample mean stands for a replicate group: {sql}"
    );
    assert!(
        sql.contains("r.is_flagged IS NOT TRUE") && sql.contains("r.unverified IS NOT TRUE"),
        "a flagged or unverified row is not a current value: {sql}"
    );
    assert!(
        sql.contains("r.time > now() - interval '30 days'"),
        "bounded to recent chunks: {sql}"
    );
    let spot_rank = sql
        .find("(r.measurement_type IS NOT DISTINCT FROM 'spot') ASC")
        .expect("spot ranks after continuous");
    let time_desc = sql.find(r#""r"."time" DESC"#).expect("newest first");
    assert!(
        spot_rank < time_desc,
        "continuous wins before newest: {sql}"
    );
}
