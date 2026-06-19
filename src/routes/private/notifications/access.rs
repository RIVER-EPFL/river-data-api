//! The single choke point for "which projects may this user be notified about".
//!
//! Today every Keycloak user can see every project, so this returns `None` (= all projects) and the
//! subscription endpoints + fan-out impose no project restriction. When role-scoped project access
//! lands, this is the ONE function that returns a bounded set — every subscription read/write and
//! every alert fan-out already routes through it, so the guard takes effect everywhere at once
//! (a user must never receive, or be able to subscribe to, alerts for a project they can't access).

use sea_orm::DatabaseConnection;
use uuid::Uuid;

/// Project ids the user `sub` may receive notifications for. `None` means "all projects" — the
/// current behaviour. A `Some(set)` will confine subscriptions and fan-out once RBAC exists.
pub async fn accessible_project_ids(_db: &DatabaseConnection, _sub: &str) -> Option<Vec<Uuid>> {
    None
}

/// Whether `project` is accessible to `sub`. `None` accessible set = all projects.
#[must_use]
pub fn project_allowed(accessible: &Option<Vec<Uuid>>, project: Uuid) -> bool {
    accessible.as_ref().is_none_or(|ids| ids.contains(&project))
}
