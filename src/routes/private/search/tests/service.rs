//! Scenario: the parameters arm of `/search` is built from the entity's declared fulltext columns.
//! Expected behaviour: a `#[crudcrate(fulltext)]` column is matched case-insensitively, the query
//! selects only the three fields the response carries, and the pattern is bound rather than pasted.

use super::{matching_parameters, matching_sites, parameters};
use crate::common::authz::AccessScope;
use sea_orm::QueryTrait;

#[test]
fn test_matching_parameters_ilikes_every_declared_fulltext_column() {
    let sql = matching_parameters("%chl%")
        .into_query()
        .to_string(sea_orm::sea_query::PostgresQueryBuilder);
    let declared =
        <parameters::Parameter as crudcrate::CRUDResource>::fulltext_searchable_columns();
    assert!(
        !declared.is_empty(),
        "parameters declares no fulltext column"
    );
    for (name, _) in declared {
        assert!(
            str::contains(&sql, &format!(r#""{name}" ILIKE '%chl%'"#)),
            "{name} is not matched: {sql}"
        );
    }
    assert!(
        str::contains(&sql, r#"ORDER BY "parameters"."code" ASC LIMIT 10"#),
        "{sql}"
    );
    assert!(
        str::starts_with(
            &sql,
            r#"SELECT "parameters"."id", "parameters"."code", "parameters"."name" FROM "parameters""#,
        ),
        "{sql}"
    );
}

/// An empty `Projects` set is a member with no grants, and must match nothing rather than
/// everything (`common/authz.rs`, fail closed).
#[test]
fn test_an_empty_project_scope_matches_no_site() {
    let scope = AccessScope::Projects(std::sync::Arc::new(std::collections::HashSet::new()));
    let sql = matching_sites("%a%", &scope)
        .into_query()
        .to_string(sea_orm::sea_query::PostgresQueryBuilder);
    assert!(
        !str::contains(&sql, "IN ()"),
        "an empty scope renders as invalid SQL: {sql}"
    );
    assert!(
        str::contains(&sql, "1 = 2") || str::contains(&sql, "FALSE"),
        "an empty scope does not fail closed: {sql}"
    );
}

/// A confined principal sees its own projects, sites in them, and sensors only through a
/// deployment at one of those sites; the sensor correlation is on the outer `sensors` row.
#[test]
fn test_a_confined_scope_filters_sites_sensors_and_projects() {
    let scope = AccessScope::one(uuid::Uuid::nil());
    let render = |query: sea_orm::sea_query::SelectStatement| {
        query.to_string(sea_orm::sea_query::PostgresQueryBuilder)
    };
    let sites = render(matching_sites("%a%", &scope).into_query());
    assert!(
        str::contains(&sites, r#""sites"."project_id" IN ("#),
        "{sites}"
    );
    let projects = render(super::matching_projects("%a%", &scope).into_query());
    assert!(
        str::contains(&projects, r#""projects"."id" IN ("#),
        "{projects}"
    );
    let sensors = render(super::matching_sensors("%a%", &scope).into_query());
    assert!(
        str::contains(
            &sensors,
            r#"EXISTS(SELECT 1 FROM "sensor_deployments" INNER JOIN "sites" ON "sites"."id" = "sensor_deployments"."site_id" WHERE "sensor_deployments"."sensor_id" = "sensors"."id" AND "sites"."project_id" IN ("#
        ),
        "{sensors}"
    );
}

/// An unconfined principal carries no project filter at all.
#[test]
fn test_an_unrestricted_scope_filters_on_the_pattern_alone() {
    for sql in [
        matching_sites("%a%", &AccessScope::Unrestricted)
            .into_query()
            .to_string(sea_orm::sea_query::PostgresQueryBuilder),
        super::matching_sensors("%a%", &AccessScope::Unrestricted)
            .into_query()
            .to_string(sea_orm::sea_query::PostgresQueryBuilder),
        super::matching_projects("%a%", &AccessScope::Unrestricted)
            .into_query()
            .to_string(sea_orm::sea_query::PostgresQueryBuilder),
    ] {
        assert!(!str::contains(&sql, "project_id"), "{sql}");
        assert!(!str::contains(&sql, "EXISTS"), "{sql}");
    }
}
