//! The four scoped catalog queries behind `/search`, and the `ILIKE` disjunction they are built
//! from.

use sea_orm::sea_query::extension::postgres::PgExpr;
use sea_orm::sea_query::{Condition, Expr, Query as SeaQuery};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, ExprTrait, QueryFilter, QueryOrder, QuerySelect,
    QueryTrait,
};

use super::models::{ParameterResult, ProjectResult, SensorResult, SiteResult};
use crate::common::authz::AccessScope;
use crate::error::AppResult;
use crate::routes::private::sensors::deployments;
use crate::routes::private::{parameters, projects, sensors, sites};

/// Matching sites, sensors, parameters and projects, ten of each.
///
/// A project-scoped key sees only its own project, that project's sites, and sensors deployed
/// there. The global measurement catalog (`parameters`) is shared reference data, not project
/// data, so it stays visible to everyone. Unscoped principals (Keycloak users, unscoped tokens)
/// search across all projects unchanged.
pub async fn matches<C: ConnectionTrait>(
    db: &C,
    pattern: &str,
    scope: &AccessScope,
) -> AppResult<(
    Vec<SiteResult>,
    Vec<SensorResult>,
    Vec<ParameterResult>,
    Vec<ProjectResult>,
)> {
    Ok(tokio::try_join!(
        matching_sites(pattern, scope)
            .into_model::<SiteResult>()
            .all(db),
        matching_sensors(pattern, scope)
            .into_model::<SensorResult>()
            .all(db),
        matching_parameters(pattern)
            .into_model::<ParameterResult>()
            .all(db),
        matching_projects(pattern, scope)
            .into_model::<ProjectResult>()
            .all(db),
    )?)
}

/// Matching sites, ten of them, ordered by name, confined to the scope's projects. A site with no
/// project is invisible to a confined principal, which is `allows_project_opt`'s rule.
fn matching_sites(pattern: &str, scope: &AccessScope) -> sea_orm::Select<sites::Entity> {
    sites::Entity::find()
        .select_only()
        .columns([sites::Column::Id, sites::Column::Name])
        .filter(fulltext_condition::<sites::Site>(pattern))
        .apply_if(scope.project_ids(), |query, ids| {
            query.filter(sites::Column::ProjectId.is_in(ids))
        })
        .order_by_asc(sites::Column::Name)
        .limit(10)
}

/// Matching sensors, ten of them, ordered by serial number. A confined principal sees a sensor
/// only through a deployment at one of its own sites.
fn matching_sensors(pattern: &str, scope: &AccessScope) -> sea_orm::Select<sensors::Entity> {
    sensors::Entity::find()
        .select_only()
        .columns([
            sensors::Column::Id,
            sensors::Column::SerialNumber,
            sensors::Column::Name,
        ])
        .filter(fulltext_condition::<sensors::Sensor>(pattern))
        .apply_if(scope.project_ids(), |query, ids| {
            query.filter(Expr::exists(
                SeaQuery::select()
                    .expr(Expr::val(1))
                    .from(deployments::Entity)
                    .inner_join(
                        sites::Entity,
                        Expr::col((sites::Entity, sites::Column::Id))
                            .equals((deployments::Entity, deployments::Column::SiteId)),
                    )
                    .and_where(
                        Expr::col((deployments::Entity, deployments::Column::SensorId))
                            .equals((sensors::Entity, sensors::Column::Id)),
                    )
                    .and_where(sites::Column::ProjectId.is_in(ids))
                    .take(),
            ))
        })
        .order_by_asc(sensors::Column::SerialNumber)
        .limit(10)
}

/// Matching projects, ten of them, ordered by name, confined to the scope's own projects.
fn matching_projects(pattern: &str, scope: &AccessScope) -> sea_orm::Select<projects::Entity> {
    projects::Entity::find()
        .select_only()
        .columns([projects::Column::Id, projects::Column::Name])
        .filter(fulltext_condition::<projects::Project>(pattern))
        .apply_if(scope.project_ids(), |query, ids| {
            query.filter(projects::Column::Id.is_in(ids))
        })
        .order_by_asc(projects::Column::Name)
        .limit(10)
}

/// Matching parameters, ten of them, ordered by code. The global measurement catalog is shared
/// reference data, so no scope filter applies.
fn matching_parameters(pattern: &str) -> sea_orm::Select<parameters::Entity> {
    parameters::Entity::find()
        .select_only()
        .columns([
            parameters::Column::Id,
            parameters::Column::Code,
            parameters::Column::Name,
        ])
        .filter(fulltext_condition::<parameters::Parameter>(pattern))
        .order_by_asc(parameters::Column::Code)
        .limit(10)
}

/// The `ILIKE` disjunction over an entity's declared fulltext columns, as a condition the query
/// builder composes. The columns come from the entity rather than from this file, so a
/// `#[crudcrate(fulltext)]` added to a model is searched here too.
fn fulltext_condition<R: crudcrate::CRUDResource>(pattern: &str) -> Condition {
    R::fulltext_searchable_columns()
        .into_iter()
        .fold(Condition::any(), |condition, (_, column)| {
            condition.add(Expr::col(column).ilike(pattern))
        })
}

#[cfg(test)]
#[path = "tests/service.rs"]
mod tests;
