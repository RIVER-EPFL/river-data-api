use super::*;
use std::collections::HashSet;
use std::sync::Arc;

fn project() -> Uuid {
    Uuid::from_u128(1)
}

fn other() -> Uuid {
    Uuid::from_u128(2)
}

fn granted() -> AccessScope {
    AccessScope::one(project())
}

fn no_grants() -> AccessScope {
    AccessScope::Projects(Arc::new(HashSet::new()))
}

#[test]
fn test_unrestricted_reaches_every_row() {
    for row in [
        RowProject::In(vec![other()]),
        RowProject::Global,
        RowProject::Unresolved,
    ] {
        assert!(in_scope(&AccessScope::Unrestricted, &row, Unowned::Deny));
    }
}

#[test]
fn test_missing_is_denied_even_when_unrestricted() {
    assert!(!in_scope(
        &AccessScope::Unrestricted,
        &RowProject::Missing,
        Unowned::Allow
    ));
}

#[test]
fn test_a_granted_project_is_in_scope() {
    assert!(in_scope(
        &granted(),
        &RowProject::In(vec![project()]),
        Unowned::Deny
    ));
    assert!(!in_scope(
        &granted(),
        &RowProject::In(vec![other()]),
        Unowned::Deny
    ));
}

#[test]
fn test_a_multi_project_row_needs_one_granted_project() {
    assert!(in_scope(
        &granted(),
        &RowProject::In(vec![other(), project()]),
        Unowned::Deny
    ));
}

#[test]
fn test_a_member_with_no_grants_sees_nothing() {
    assert!(!in_scope(
        &no_grants(),
        &RowProject::In(vec![project()]),
        Unowned::Deny
    ));
    assert!(!in_scope(&no_grants(), &RowProject::Global, Unowned::Deny));
    assert!(in_scope(&no_grants(), &RowProject::Global, Unowned::Allow));
}

#[test]
fn test_unowned_policy_governs_global_and_unresolved() {
    for row in [RowProject::Global, RowProject::Unresolved] {
        assert!(!in_scope(&granted(), &row, Unowned::Deny));
        assert!(in_scope(&granted(), &row, Unowned::Allow));
    }
}

#[test]
fn test_row_guard_is_404_and_target_guard_is_403() {
    let row = RowProject::In(vec![other()]);
    assert!(matches!(
        require_row_in_scope(&granted(), &row, Unowned::Deny, "job"),
        Err(AppError::NotFound(_))
    ));
    assert!(matches!(
        require_target_in_scope(&granted(), &row, Unowned::Deny, "site"),
        Err(AppError::Forbidden(_))
    ));
    assert!(
        require_row_in_scope(
            &granted(),
            &RowProject::In(vec![project()]),
            Unowned::Deny,
            "job"
        )
        .is_ok()
    );
}

#[test]
fn test_project_filter_sql_numbers_the_placeholder_after_existing_values() {
    let mut values: Vec<sea_orm::Value> = vec![42i32.into()];
    let predicate = project_filter_sql(&granted(), "s.project_id", &mut values);
    assert_eq!(predicate.as_deref(), Some("s.project_id = ANY($2)"));
    assert_eq!(values.len(), 2);
}

#[test]
fn test_project_filter_sql_is_absent_for_an_unrestricted_caller() {
    let mut values: Vec<sea_orm::Value> = Vec::new();
    assert!(project_filter_sql(&AccessScope::Unrestricted, "s.project_id", &mut values).is_none());
    assert!(values.is_empty());
}

#[test]
fn test_project_filter_sql_binds_an_empty_set_for_a_member_with_no_grants() {
    let mut values: Vec<sea_orm::Value> = Vec::new();
    assert_eq!(
        project_filter_sql(&no_grants(), "s.project_id", &mut values).as_deref(),
        Some("s.project_id = ANY($1)")
    );
    assert_eq!(values.len(), 1);
}

/// Scenario: a restricted caller's project filter goes into a sea-query statement rather than
/// into a string the caller assembles.
///
/// Expected behaviour: it renders as a predicate Postgres accepts. `= ANY(...)` is one operator
/// and a parenthesised right-hand side is a syntax error, which is what the built form produced
/// while every caller was still assembling the text by hand.
#[test]
fn test_the_project_filter_renders_a_predicate_postgres_parses() {
    use sea_orm::sea_query::{PostgresQueryBuilder, Query};
    let predicate = project_filter(&granted(), sea_orm::sea_query::Alias::new("project_id"))
        .expect("a restricted caller has a filter");
    let (sql, _) = Query::select()
        .expr(sea_orm::sea_query::Expr::cust("1"))
        .from(sea_orm::sea_query::Alias::new("sites"))
        .and_where(predicate)
        .build(PostgresQueryBuilder);
    assert!(
        !sql.contains("(ANY("),
        "Postgres rejects a parenthesised ANY on the right of `=`: {sql}"
    );
    assert!(
        sql.contains("\"project_id\" IN ("),
        "the caller's projects confine the column: {sql}"
    );
}

/// Scenario: a caller with the role but no grant at all.
///
/// Expected behaviour: the filter still renders, and it matches nothing. Failing open here is
/// the whole surface, so the empty set is the case worth pinning rather than the populated one.
#[test]
fn test_a_caller_with_no_grants_is_filtered_to_nothing() {
    use sea_orm::sea_query::{PostgresQueryBuilder, Query};
    let predicate = project_filter(&no_grants(), sea_orm::sea_query::Alias::new("project_id"))
        .expect("a caller with no grants is still restricted");
    let (sql, values) = Query::select()
        .expr(sea_orm::sea_query::Expr::cust("1"))
        .from(sea_orm::sea_query::Alias::new("sites"))
        .and_where(predicate)
        .build(PostgresQueryBuilder);
    // The builder writes an empty `IN` as a bound contradiction rather than as `IN ()`, which
    // is not valid SQL. Either way the query returns nothing, which is the direction to fail in.
    assert_eq!(
        format!("{values:?}"),
        "Values([Int(Some(1)), Int(Some(2))])",
        "an empty grant set is a contradiction, not an open filter: {sql} {values:?}"
    );
}
