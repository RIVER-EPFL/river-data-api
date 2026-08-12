//! Live role resolution for Telegram chats, the anti-backdoor core.
//!
//! A linked chat is only a delivery address; its authority is the linked Keycloak user's *current*
//! state. Every command resolves the user live (enabled flag + realm roles) through the Keycloak
//! admin proxy, with a short TTL cache to absorb bursts. Resolution fails closed: if Keycloak can't
//! be reached the command is denied, but the identity is NOT deactivated (so an outage can't mass
//! unlink). A definitive negative, user gone, disabled, or holding no riverdata role, is
//! `Revoked`, which deactivates the identity.

use std::time::Duration;

use moka::future::Cache;

use crate::common::AppState;
use crate::common::authz::Role;
use crate::routes::private::admin::users;

/// The live authority for a linked chat. `Active` carries the user's current highest riverdata role
/// (so command gates can compare levels); `Revoked` means the link must be deactivated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoleResolution {
    Active(Role),
    Revoked,
}

impl RoleResolution {
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

    /// At least `min`'s access level, e.g. `Role::River` for data writes like `/grab`, matching the
    /// HTTP `WriteData` capability so the bot can't be a lower-privilege side door.
    #[must_use]
    pub fn allows_level(&self, min: &Role) -> bool {
        matches!(self, Self::Active(r) if r.level() >= min.level())
    }

    /// The resolved role, if the user is active.
    #[must_use]
    pub fn role(&self) -> Option<&Role> {
        match self {
            Self::Active(r) => Some(r),
            Self::Revoked => None,
        }
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

    /// Resolve a Keycloak sub's current role. `None` = unavailable (deny but don't deactivate).
    pub async fn resolve(&self, state: &AppState, sub: &str) -> Option<RoleResolution> {
        if let Some(cached) = self.cache.get(sub).await {
            return Some(cached);
        }
        let resolved = resolve_live(state, sub).await?;
        self.cache.insert(sub.to_string(), resolved.clone()).await;
        Some(resolved)
    }

    /// Drop a sub's cached role so the next command re-resolves immediately. Called when roles
    /// change through the user-management proxy.
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
    // The user's highest riverdata role governs; a non-riverdata realm role (level 0) is revoked.
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
