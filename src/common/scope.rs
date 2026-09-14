//! Project-scope confinement for plain handlers.
//!
//! CRUD routes are confined automatically (`inject_project_scope`). Every
//! other handler confines itself, and the default when it does not is to serve everything. These are
//! the three shapes a handler needs, so confining is one call:
//!
//! - enumerating rows: [`scope_site_ids`] for a `site_id = ANY($n)` filter, or [`project_filter`]
//!   when the query already joins `sites`;
//! - a body-supplied target: [`require_sites_in_scope`], 403 outside the caller's projects;
//! - an id-addressed row: [`require_row_in_scope`], 404 outside the caller's projects, over a
//!   [`RowProject`] from one of the `project_of_*` resolvers.
//!
//! `reprocessing_jobs` and `alarm_events` carry no project column; [`project_of_job`] and
//! [`project_of_alarm_event`] resolve one the way `crud_read_scope_condition` does, through the site.
//!
//! A row that resolves to more than one project (a sensor deployed across projects) is in scope when
//! *any* of them is, matching how the same rows are filtered on read. A write that must hold every
//! project goes through [`require_sites_in_scope`].

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, FromQueryResult, Statement};
use uuid::Uuid;

use crate::common::authz::AccessScope;
use crate::error::{AppError, AppResult};

pub use crate::common::middleware::scope_site_ids;

/// The projects a single row belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowProject {
    /// The row exists and belongs to these projects (never empty).
    In(Vec<Uuid>),
    /// The row exists and has no project-bearing target at all (a global job).
    Global,
    /// The row exists and names a target, but that target belongs to no project (a never-deployed
    /// sensor, a site with no project).
    Unresolved,
    /// No such row.
    Missing,
}

/// How a restricted caller is treated for a row no project can be resolved for
/// ([`RowProject::Global`] and [`RowProject::Unresolved`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unowned {
    /// Fail closed. The default for anything that names a project-bearing entity.
    Deny,
    /// Let it through, for rows that legitimately have no project: a global job's timeline, a
    /// sensor sitting in inventory before its first deployment.
    Allow,
}

/// Confine an id-addressed row to the caller's projects. Out of scope and missing are both 404, so
/// the response does not confirm the row exists.
pub fn require_row_in_scope(
    scope: &AccessScope,
    row: &RowProject,
    unowned: Unowned,
    what: &str,
) -> AppResult<()> {
    in_scope(scope, row, unowned)
        .then_some(())
        .ok_or_else(|| AppError::NotFound(format!("{what} not found")))
}

/// Confine a body-supplied target to the caller's projects, 403 outside it.
pub fn require_target_in_scope(
    scope: &AccessScope,
    row: &RowProject,
    unowned: Unowned,
    what: &str,
) -> AppResult<()> {
    if in_scope(scope, row, unowned) {
        return Ok(());
    }
    Err(AppError::Forbidden(format!(
        "That {what} is outside your project access"
    )))
}

/// The one rule both guards apply.
fn in_scope(scope: &AccessScope, row: &RowProject, unowned: Unowned) -> bool {
    match row {
        RowProject::Missing => false,
        _ if !scope.is_restricted() => true,
        RowProject::Global | RowProject::Unresolved => unowned == Unowned::Allow,
        RowProject::In(projects) => projects.iter().any(|p| scope.allows_project(*p)),
    }
}

/// Reject a restricted caller writing to any site outside its projects. Every named site must be in
/// scope; an unknown site is rejected too.
pub async fn require_sites_in_scope(
    db: &DatabaseConnection,
    scope: &AccessScope,
    site_ids: &[Uuid],
) -> AppResult<()> {
    crate::common::middleware::enforce_project_scope_for_sites(db, scope, site_ids).await
}

/// The caller's projects as a predicate on `column`, `None` when the caller is unrestricted, which
/// is a predicate the query then omits.
#[must_use]
pub fn project_filter(
    scope: &AccessScope,
    column: impl sea_orm::sea_query::IntoColumnRef,
) -> Option<sea_orm::sea_query::Expr> {
    use sea_orm::sea_query::ExprTrait;
    // `IN` rather than `= ANY(array)`: the builder parenthesises the right-hand side of `eq`,
    // and `= (ANY($1))` is a syntax error. The two mean the same thing to the planner.
    let projects = scope.project_ids()?;
    Some(sea_orm::sea_query::Expr::col(column).is_in(projects))
}

/// The projects a tracked job belongs to: its `site_id`, else every project its `sensor_id` is
/// deployed into. A job with neither is [`RowProject::Global`].
pub async fn project_of_job(db: &DatabaseConnection, job_id: Uuid) -> AppResult<RowProject> {
    resolve(
        db,
        "SELECT (j.site_id IS NULL AND j.sensor_id IS NULL) AS untargeted, s.project_id \
         FROM reprocessing_jobs j \
         LEFT JOIN sites s ON s.id = j.site_id \
         WHERE j.id = $1 \
         UNION ALL \
         SELECT false AS untargeted, s.project_id \
         FROM reprocessing_jobs j \
         JOIN sensor_deployments d ON d.sensor_id = j.sensor_id \
         JOIN sites s ON s.id = d.site_id \
         WHERE j.id = $1 AND j.site_id IS NULL",
        job_id,
    )
    .await
}

/// The project an alarm event belongs to, through its site.
pub async fn project_of_alarm_event(
    db: &DatabaseConnection,
    event_id: Uuid,
) -> AppResult<RowProject> {
    resolve(
        db,
        "SELECT false AS untargeted, s.project_id \
         FROM alarm_events ae \
         LEFT JOIN sites s ON s.id = ae.site_id \
         WHERE ae.id = $1",
        event_id,
    )
    .await
}

/// The project a site belongs to.
pub async fn project_of_site(db: &DatabaseConnection, site_id: Uuid) -> AppResult<RowProject> {
    resolve(
        db,
        "SELECT false AS untargeted, s.project_id FROM sites s WHERE s.id = $1",
        site_id,
    )
    .await
}

/// The project a site parameter belongs to, through its site.
pub async fn project_of_site_parameter(
    db: &DatabaseConnection,
    site_parameter_id: Uuid,
) -> AppResult<RowProject> {
    resolve(
        db,
        "SELECT false AS untargeted, s.project_id \
         FROM site_parameters sp \
         JOIN sites s ON s.id = sp.site_id \
         WHERE sp.id = $1",
        site_parameter_id,
    )
    .await
}

/// The projects a sensor is deployed into. A sensor with no deployment is
/// [`RowProject::Unresolved`], ie. inventory whose owner is not decided yet: pass
/// [`Unowned::Allow`] where a manager is meant to reach an undeployed instrument.
pub async fn project_of_sensor(db: &DatabaseConnection, sensor_id: Uuid) -> AppResult<RowProject> {
    resolve(
        db,
        "SELECT false AS untargeted, s.project_id \
         FROM sensors sn \
         LEFT JOIN sensor_deployments d ON d.sensor_id = sn.id \
         LEFT JOIN sites s ON s.id = d.site_id \
         WHERE sn.id = $1",
        sensor_id,
    )
    .await
}

/// One row of a resolver query: whether the row targets nothing, and the project it reaches.
#[derive(FromQueryResult)]
struct ScopeRow {
    untargeted: bool,
    project_id: Option<Uuid>,
}

/// Run a resolver query returning `(untargeted, project_id)` rows and classify the result.
async fn resolve(db: &DatabaseConnection, sql: &str, id: Uuid) -> AppResult<RowProject> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            [id.into()],
        ))
        .await?;
    if rows.is_empty() {
        return Ok(RowProject::Missing);
    }
    // A row this resolver cannot read is not a row that belongs to no project: the caller's reach
    // would then be decided by which default was typed. Every query above selects `untargeted` as
    // a boolean and `project_id` as a nullable uuid, so a failure to decode either is the query
    // and its resolver disagreeing, and it is answered as an error.
    let resolved = rows
        .iter()
        .map(|r| ScopeRow::from_query_result(r, ""))
        .collect::<Result<Vec<_>, _>>()?;
    let mut projects: Vec<Uuid> = resolved.iter().filter_map(|r| r.project_id).collect();
    projects.sort_unstable();
    projects.dedup();
    if !projects.is_empty() {
        return Ok(RowProject::In(projects));
    }
    Ok(if resolved.iter().any(|r| r.untargeted) {
        RowProject::Global
    } else {
        RowProject::Unresolved
    })
}

#[cfg(test)]
#[path = "tests/scope.rs"]
mod tests;

// --- Project scope on the operator actions ---
//
// Three rules, applied by every operator action:
//
// 1. A named site is confined with `require_sites_in_scope` (403), matching `preview_derived` and
//    the ingestion write paths.
// 2. A named row (a sensor, a deployment) is confined with `confine_target` (404 when no such row,
//    403 when it exists outside the caller's grants).
// 3. An action that names nothing runs against every project, so a restricted caller must name a
//    target: `require_named_target` refuses it. Administrators, unscoped tokens and sync tokens are
//    unrestricted and unaffected.
//
// `reconcile_alarms` is the one exception to rule 3, and the reason is what it writes: it takes no
// target because it touches no stored measurement or history. It recomputes derived state (the
// open-alarm set) that the sweeper already recomputes on its own cadence, so a member triggering
// one changes nothing they could not obtain by waiting.
//
// `deny_scoped_token` on the route group stops a project-scoped API TOKEN before any of this; it
// was never a check on granted members, who reach these handlers as `AccessScope::Projects`.

/// Confine an action's named row to the caller's projects.
///
/// A row that does not exist is 404 for everyone, including an administrator: the action has
/// nothing to act on. A row outside a restricted caller's grants is 403, the same answer the route
/// already gives a project-scoped token, and the enumerations that could hand out such an id are
/// confined by the same scope.
pub fn confine_target(
    scope: &AccessScope,
    row: &RowProject,
    unowned: Unowned,
    what: &str,
) -> AppResult<()> {
    if matches!(row, RowProject::Missing) {
        return Err(AppError::NotFound(format!("{what} not found")));
    }
    require_target_in_scope(scope, row, unowned, what)
}

/// Refuse an untargeted run to a restricted caller: with nothing named, the action reaches every
/// project. `named` is whether the request identified something narrower than the whole
/// installation; `what` names what to pass instead. A request that names nothing *and* asks for
/// nothing keeps its existing 400, which is a bad request rather than a scope answer.
pub fn require_named_target(scope: &AccessScope, named: bool, what: &str) -> AppResult<()> {
    if named || !scope.is_restricted() {
        return Ok(());
    }
    Err(AppError::Forbidden(format!(
        "Name the {what} this action should touch; an unnamed target is not confined to your projects"
    )))
}
