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
            latest_served_query(false, Expr::cust("$1"), Expr::cust("$2"))
                .to_string(PostgresQueryBuilder),
            false,
        ),
        (
            "latest_served_query spot",
            latest_served_query(true, Expr::cust("$1"), Expr::cust("$2"))
                .to_string(PostgresQueryBuilder),
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

/// Expected behaviour: the episode statement keeps the four-step chain, each window function in
/// its own step, and closes a run only where the following in-range reading exists.
#[test]
fn test_the_episode_statement_walks_ordered_scored_marked_runs() {
    let sql = super::episodes_query("CASE WHEN v > 1 THEN 2 ELSE 0 END", false)
        .to_string(PostgresQueryBuilder);
    let steps: Vec<usize> = [
        "\"ordered\" AS",
        "\"scored\" AS",
        "\"marked\" AS",
        "\"runs\" AS",
    ]
    .iter()
    .map(|step| {
        sql.find(step)
            .unwrap_or_else(|| panic!("{step} missing: {sql}"))
    })
    .collect();
    assert!(steps.windows(2).all(|w| w[0] < w[1]), "in order: {sql}");
    assert!(
        sql.contains("ROWS UNBOUNDED PRECEDING"),
        "the run id is a running sum over every earlier instant: {sql}"
    );
    assert!(
        sql.contains("HAVING (ARRAY_AGG(next_t ORDER BY t DESC))[1] IS NOT NULL"),
        "a run still breaching at the window edge is not an episode: {sql}"
    );
}

/// Expected behaviour: the per-parameter count reads the violations select itself, so a count and
/// the export it gates cannot disagree.
#[test]
fn test_the_violation_count_groups_the_violations_select() {
    let site = Uuid::nil();
    let counts = super::violation_counts_query(site, 1).to_string(PostgresQueryBuilder);
    let violations = violations_query(site, None, 1).to_string(PostgresQueryBuilder);
    let select = violations
        .find(r#"SELECT "sv"."parameter_id""#)
        .expect("the violations select");
    assert!(
        counts.contains(&violations[select..]),
        "the count carries the violations select verbatim: {counts}"
    );
    assert!(
        counts.contains(r#"COUNT(*) AS "n""#) && counts.contains(r#"GROUP BY "v"."parameter_id""#),
        "one row per parameter: {counts}"
    );
}
