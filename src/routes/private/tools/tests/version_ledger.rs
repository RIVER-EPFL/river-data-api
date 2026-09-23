use crate::routes::private::tools::models::{Engine, PinnedFormula};
use crate::routes::private::tools::service::{render, version_formulas, version_ledger_sql};

fn pco2(expr: &str) -> PinnedFormula {
    PinnedFormula {
        code: "pco2".to_string(),
        label: "pCO2".to_string(),
        units: Some("uatm".to_string()),
        formula: expr.to_string(),
        ordinal: 0,
        output_parameter_code: Some("pCO2".to_string()),
        sources: vec![
            ("kh".to_string(), "KH".to_string()),
            ("p".to_string(), "Pressure".to_string()),
        ],
        held: Vec::new(),
        site_sources: Vec::new(),
        curve_slot: None,
        per_replicate: None,
        intermediate: false,
    }
}

/// Scenario: a formula row was edited through CRUD after version 4 was minted, so the live rows
/// read `kh * p * 1.1` while version 4's body holds `kh * p`.
///
/// Expected behaviour: a run of version 4 evaluates version 4's body, so the stamp it carries
/// names the arithmetic it did.
#[test]
fn a_run_evaluates_the_body_of_the_version_it_stamps() {
    let pinned = vec![pco2("kh * p")];
    let live = vec![pco2("kh * p * 1.1")];
    let body = render(&pinned).expect("the set has an order");
    let formulas = version_formulas("pco2_calc", Engine::Formula, &body).expect("the body reads");
    assert_eq!(formulas, pinned);
    assert_ne!(formulas, live);
}

#[test]
fn a_script_calculation_evaluates_no_formulas() {
    let formulas = version_formulas("doc", Engine::Script, "tool <- function(inputs) list()")
        .expect("a script body is not read as formulas");
    assert!(formulas.is_empty());
}

#[test]
fn an_unreadable_formula_body_names_the_calculation() {
    let err = version_formulas("pco2_calc", Engine::Formula, "")
        .expect_err("an empty body is no formula set");
    assert!(err.to_string().contains("pco2_calc"), "{err}");
}

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

// --- Built statements ---

#[test]
fn the_active_tool_query_joins_each_calculation_to_its_active_version() {
    use sea_orm::sea_query::PostgresQueryBuilder;
    let sql =
        crate::routes::private::tools::service::active_tool_query().to_string(PostgresQueryBuilder);
    assert!(
        sql.contains(
            r#"INNER JOIN "tool_script_versions" AS "v" ON "v"."id" = "s"."active_version_id""#
        ),
        "{sql}"
    );
    assert!(sql.contains(r#""s"."id" AS "script_id""#), "{sql}");
    assert!(sql.contains(r#""v"."id" AS "version_id""#), "{sql}");
    assert!(sql.contains(r#""v"."content_hash""#), "{sql}");
}

#[test]
fn the_own_formulas_query_reads_the_calculations_formulas_in_evaluation_order() {
    use sea_orm::{DbBackend, QueryTrait};
    let id = uuid::Uuid::nil();
    let sql = crate::routes::private::tools::service::own_formulas_query(&[id])
        .build(DbBackend::Postgres)
        .to_string();
    assert!(sql.contains(r#"FROM "calculation_formulas""#), "{sql}");
    assert!(
        sql.contains(r#""calculation_formulas"."tool_script_id" IN ("#),
        "{sql}"
    );
    assert!(
        sql.ends_with(
            r#"ORDER BY "calculation_formulas"."ordinal" ASC, "calculation_formulas"."code" ASC"#
        ),
        "{sql}"
    );
}

#[test]
fn the_latest_version_query_takes_the_calculations_highest_number() {
    use sea_orm::{DbBackend, QueryTrait};
    let sql = crate::routes::private::tools::service::latest_version_no_query(uuid::Uuid::nil())
        .build(DbBackend::Postgres)
        .to_string();
    assert!(
        sql.starts_with(r#"SELECT MAX("tool_script_versions"."version_no") AS "latest""#),
        "{sql}"
    );
    assert!(
        sql.contains(r#"WHERE "tool_script_versions"."tool_script_id" = "#),
        "{sql}"
    );
}

#[test]
fn a_first_version_is_numbered_one_and_the_next_follows_the_latest() {
    use crate::routes::private::tools::service::next_version_no;
    assert_eq!(next_version_no(None), 1);
    // 4 + 1
    assert_eq!(next_version_no(Some(4)), 5);
}
