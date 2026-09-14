use sea_orm::sea_query::{PostgresQueryBuilder, Query};

use super::{AccessScope, deployed_in_scope, site_in_scope};

fn sql(expr: sea_orm::sea_query::Expr) -> String {
    Query::select().expr(expr).to_string(PostgresQueryBuilder)
}

fn restricted() -> AccessScope {
    AccessScope::Projects(std::sync::Arc::new(
        [uuid::Uuid::nil()].into_iter().collect(),
    ))
}

/// Scenario: a restricted caller enumerates calibration candidates.
/// Expected behaviour: each confinement is an EXISTS over the built tables, with the caller's
/// projects bound as values rather than spliced into text, and the correlation reaching the
/// outer `r` row.
#[test]
fn test_deployed_in_scope_confines_through_the_deployment_s_site() {
    let sql = sql(deployed_in_scope(&restricted()).expect("a restricted caller is confined"));
    assert!(sql.contains(r#"FROM "sensor_deployments" AS "d""#), "{sql}");
    assert!(sql.contains(r#"INNER JOIN "sites" AS "s""#), "{sql}");
    assert!(
        sql.contains(r#""d"."sensor_id" = "r"."sensor_id""#),
        "correlated to the reading being enumerated: {sql}"
    );
    assert!(sql.contains(r#""s"."project_id" IN"#), "{sql}");
}

#[test]
fn test_site_in_scope_confines_through_the_reading_s_own_site() {
    let sql = sql(site_in_scope(&restricted()).expect("a restricted caller is confined"));
    assert!(sql.contains(r#"FROM "sites" AS "s""#), "{sql}");
    assert!(sql.contains(r#""s"."id" = "r"."site_id""#), "{sql}");
    assert!(sql.contains(r#""s"."project_id" IN"#), "{sql}");
}

/// An unrestricted caller gets no predicate at all, which is what lets the query omit it.
#[test]
fn test_an_unrestricted_caller_is_confined_by_nothing() {
    assert!(deployed_in_scope(&AccessScope::Unrestricted).is_none());
    assert!(site_in_scope(&AccessScope::Unrestricted).is_none());
}
