//! Project-scope confinement for plain handlers.
//!
//! CRUD routes are confined automatically (`enforce_scope_on_crud`, `inject_read_scope`). Every
//! other handler confines itself, and the default when it does not is to serve everything. These are
//! the three shapes a handler needs, so confining is one call:
//!
//! - enumerating rows: [`scope_site_ids`] for a `site_id = ANY($n)` filter, or [`project_filter_sql`]
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

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
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

/// Push the caller's project set onto `values` and return the SQL predicate confining `column` to
/// it, or `None` when the caller is unrestricted (the caller then omits the predicate). `column` is
/// the query's own `project_id` expression, ie. `s.project_id`.
#[must_use]
pub fn project_filter_sql(
    scope: &AccessScope,
    column: &str,
    values: &mut Vec<sea_orm::Value>,
) -> Option<String> {
    let projects = scope.sql_project_array()?;
    values.push(projects);
    Some(format!("{column} = ANY(${})", values.len()))
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

/// Run a resolver query returning `(untargeted, project_id)` rows and classify the result.
async fn resolve(db: &DatabaseConnection, sql: &str, id: Uuid) -> AppResult<RowProject> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            [id.into()],
        ))
        .await?;
    if rows.is_empty() {
        return Ok(RowProject::Missing);
    }
    let mut projects: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| r.try_get::<Option<Uuid>>("", "project_id").ok().flatten())
        .collect();
    projects.sort_unstable();
    projects.dedup();
    if !projects.is_empty() {
        return Ok(RowProject::In(projects));
    }
    let untargeted = rows
        .iter()
        .any(|r| r.try_get::<bool>("", "untargeted").unwrap_or(false));
    Ok(if untargeted {
        RowProject::Global
    } else {
        RowProject::Unresolved
    })
}

#[cfg(test)]
mod tests {
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
        assert!(
            project_filter_sql(&AccessScope::Unrestricted, "s.project_id", &mut values).is_none()
        );
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
}
