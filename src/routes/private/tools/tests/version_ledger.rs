use crate::routes::private::tools::service::version_ledger_sql;

/// Scenario: a calculation has computed on a stream under two versions, and a reader asks what
/// each version produced.
///
/// Expected behaviour: the statement groups the curation ledger by the version a computed value
/// names, over the two kinds that carry a computation, counting reading keys rather than
/// decisions. A stream pass mints no run, so this grouping is the only thing that can answer.
#[test]
fn the_ledger_groups_computations_by_the_version_they_name() {
    let sql = version_ledger_sql();
    assert!(
        sql.contains("d.new ->> 'derived_version_id' = v.id::text"),
        "the grouping key is the version the decision names: {sql}"
    );
    assert!(
        sql.contains("d.kind IN ('derived_computed', 'formula_transition')"),
        "only the kinds that carry a computation count: {sql}"
    );
    assert!(
        sql.contains("COUNT(DISTINCT (d.stream_id, d.time, d.replicate_index))"),
        "a reading moved twice under one version is one reading: {sql}"
    );
}

/// A version with no decision joins to a row of NULL columns, which is not itself NULL: counted
/// without a filter it reads as one reading the version never made.
#[test]
fn a_version_with_no_decision_counts_nothing_rather_than_the_empty_join_row() {
    assert!(
        version_ledger_sql().contains("FILTER (WHERE d.stream_id IS NOT NULL)"),
        "the count is held to the rows a decision is actually on: {}",
        version_ledger_sql()
    );
}

/// A version that computed nothing is a row of zeros rather than an absence: "nothing stored" and
/// "not counted" are different claims, and the ledger is the set's history whole.
#[test]
fn a_version_that_computed_nothing_is_still_a_row() {
    let sql = version_ledger_sql();
    assert!(
        sql.contains("FROM tool_script_versions v")
            && sql.contains("LEFT JOIN reading_decisions d"),
        "every version is a row, joined to whatever it left: {sql}"
    );
    assert!(
        sql.contains("ORDER BY v.version_no DESC"),
        "newest version first: {sql}"
    );
}

/// The span a version covers has two ends that answer different questions: where the values sit
/// (`time`) and when the computing happened (`at`).
#[test]
fn the_row_spans_both_the_instants_and_the_computing() {
    let sql = version_ledger_sql();
    for expected in [
        "MIN(d.time) AS first_instant",
        "MAX(d.time) AS last_instant",
        "MIN(d.at) AS first_computed",
        "MAX(d.at) AS last_computed",
    ] {
        assert!(sql.contains(expected), "{expected} missing: {sql}");
    }
}
