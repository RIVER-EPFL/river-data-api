//! The single choke point for "which projects may this user be notified about".
//!
//! A member is confined to their `user_project_grants` set; an administrator is unrestricted. The
//! caller's authority is resolved live through the same [`Authorizer`](super::authz::Authorizer) the
//! bot and reconcile sweep use, so a role change takes effect within the authorizer's short TTL and a
//! Keycloak outage fails closed (the member receives nothing rather than everything). Every
//! subscription read/write and every alert fan-out routes through here, so the guard is enforced in
//! one place.

use std::collections::HashSet;

use uuid::Uuid;

use super::authz::RoleResolution;
use crate::common::AppState;
use crate::common::authz::Role;
use crate::common::grants::load_grants;

/// Project ids `sub` may be notified for. `None` = unrestricted (administrators). `Some(set)` confines
/// a member to their granted projects; an empty set — a member with no grants, or a revoked/
/// unresolvable user — receives nothing (fail closed).
pub async fn accessible_project_ids(state: &AppState, sub: &str) -> Option<HashSet<Uuid>> {
    match state.authorizer.resolve(state, sub).await {
        Some(RoleResolution::Active(role)) if role == Role::Administrator => None,
        Some(RoleResolution::Active(_)) => {
            Some((*load_grants(&state.db, &state.grants_cache, sub).await).clone())
        }
        // Revoked, or Keycloak unavailable: confine to nothing rather than risk over-delivery.
        Some(RoleResolution::Revoked) | None => Some(HashSet::new()),
    }
}

/// Whether `project` is accessible given a resolved set. `None` (unrestricted) allows everything.
#[must_use]
pub fn project_allowed(accessible: &Option<HashSet<Uuid>>, project: Uuid) -> bool {
    accessible.as_ref().is_none_or(|ids| ids.contains(&project))
}
