//! The single choke point for "which projects may this user be notified about" and for live role
//! resolution. The `Authorizer` resolves a Keycloak sub's current role with a short TTL cache and
//! lives here because every notification fan-out path routes through `accessible_project_ids`.

use std::collections::HashSet;
use std::time::Duration;

use moka::future::Cache;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::authz::Role;
use crate::common::grants::load_grants;
use crate::routes::private::admin::users;

/// The live authority for a notification recipient. `Active` carries the user's current highest
/// riverdata role; `Revoked` means the link or subscription must be deactivated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoleResolution {
    Active(Role),
    Revoked,
}

impl RoleResolution {
    /// The resolved role, if the user is active.
    #[must_use]
    pub fn role(&self) -> Option<&Role> {
        match self {
            Self::Active(r) => Some(r),
            Self::Revoked => None,
        }
    }

    /// Read commands: any current riverdata user (Intern and up).
    #[must_use]
    pub fn allows_user(&self) -> bool {
        matches!(self, Self::Active(_))
    }

    /// Operational commands (mutes): Administrator only.
    #[must_use]
    pub fn allows_admin(&self) -> bool {
        matches!(self, Self::Active(Role::Administrator))
    }

    /// At least `min`'s access level.
    #[must_use]
    pub fn allows_level(&self, min: &Role) -> bool {
        matches!(self, Self::Active(r) if r.level() >= min.level())
    }
}

/// Caches resolved roles for a short TTL. `resolve` returns `None` when Keycloak is unavailable
/// (fail closed) and `Some(Revoked)` for a definitive negative.
pub struct Authorizer {
    cache: Cache<String, RoleResolution>,
}

impl Default for Authorizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Authorizer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cache: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(Duration::from_secs(60))
                .build(),
        }
    }

    pub async fn resolve(&self, state: &AppState, sub: &str) -> Option<RoleResolution> {
        if let Some(cached) = self.cache.get(sub).await {
            return Some(cached);
        }
        let resolved = resolve_live(state, sub).await?;
        self.cache.insert(sub.to_string(), resolved.clone()).await;
        Some(resolved)
    }

    pub async fn invalidate(&self, sub: &str) {
        self.cache.invalidate(sub).await;
    }
}

async fn resolve_live(state: &AppState, sub: &str) -> Option<RoleResolution> {
    let token = users::get_admin_token(state).await.ok()?;
    let client = users::admin_client(state).ok()?;
    let base = users::admin_base_url(state).ok()?;

    let resp = client
        .http_client
        .get(format!("{base}/users/{sub}"))
        .bearer_auth(&token)
        .send()
        .await
        .ok()?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Some(RoleResolution::Revoked);
    }
    if !resp.status().is_success() {
        return None;
    }
    let user: serde_json::Value = resp.json().await.ok()?;
    if user["enabled"].as_bool() != Some(true) {
        return Some(RoleResolution::Revoked);
    }

    let roles_resp = client
        .http_client
        .get(format!("{base}/users/{sub}/role-mappings/realm"))
        .bearer_auth(&token)
        .send()
        .await
        .ok()?;
    if !roles_resp.status().is_success() {
        return None;
    }
    let roles: Vec<serde_json::Value> = roles_resp.json().await.ok()?;
    let best = roles
        .iter()
        .filter_map(|r| r["name"].as_str())
        .map(|n| Role::from(n.to_string()))
        .max_by_key(Role::level);
    match best {
        Some(role) if role.grants_access() => Some(RoleResolution::Active(role)),
        _ => Some(RoleResolution::Revoked),
    }
}

/// Project ids `sub` may be notified for. `None` = unrestricted (administrators). `Some(set)` confines
/// a member to their granted projects; an empty set, a member with no grants, or a revoked/
/// unresolvable user, receives nothing (fail closed).
pub async fn accessible_project_ids(state: &AppState, sub: &str) -> Option<HashSet<Uuid>> {
    match state.authorizer.resolve(state, sub).await {
        Some(RoleResolution::Active(role)) if role == Role::Administrator => None,
        Some(RoleResolution::Active(_)) => {
            Some((*load_grants(&state.db, &state.grants_cache, sub).await).clone())
        }
        Some(RoleResolution::Revoked) | None => Some(HashSet::new()),
    }
}

/// Whether `project` is accessible given a resolved set. `None` (unrestricted) allows everything.
#[must_use]
pub fn project_allowed(accessible: &Option<HashSet<Uuid>>, project: Uuid) -> bool {
    accessible.as_ref().is_none_or(|ids| ids.contains(&project))
}
