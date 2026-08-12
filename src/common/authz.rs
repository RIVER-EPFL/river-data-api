//! All authorization POLICY in one module: roles, capabilities, the role→capability matrix, and
//! the token-bit mapping. `middleware.rs` keeps transport (JWT extraction, `AuthContext`
//! construction, layer plumbing) and delegates every "may X do Y?" decision here.
//!
//! ## Model
//! Keycloak realm roles form ordered access levels, Intern < River < Manager < Administrator,
//! and each [`Capability`] names the minimum level that holds it ([`Capability::min_role`], the
//! single readable policy table). The RIVER realm is EPFL-federated and auto-assigns
//! `default-roles-river` to every login, so authentication alone proves nothing: a user with no
//! `riverdata-*` role (level 0) is denied everything.
//!
//! API tokens are NOT levelled: they keep their four independent permission bits with frozen
//! semantics ([`TokenBit`]). Where a route's Keycloak level moved (e.g. stream registration became
//! Administrator-only for humans) the route keeps its historical token bit via an explicit
//! [`TokenAccess`] override, so sync-service tokens are unaffected by the human-side RBAC.

use std::collections::HashSet;
use std::sync::Arc;

use axum::{extract::Request, middleware::Next, response::{IntoResponse, Response}};
use serde::Deserialize;
use uuid::Uuid;

use crate::error::AppError;

/// The set of projects an identity may see and act in. `Unrestricted` = global access (Keycloak
/// administrators, unscoped API tokens, sync session tokens); `Projects` = confined to a set (a
/// project-scoped API token carries exactly one; a non-admin Keycloak user carries their grant
/// set). One type serves both identities so scope filtering is written once. An empty `Projects`
/// set (a member with no grants) correctly filters everything out, fail closed.
#[derive(Clone, Debug)]
pub enum AccessScope {
    Unrestricted,
    Projects(Arc<HashSet<Uuid>>),
}

impl AccessScope {
    /// A single-project scope (a scoped API token).
    #[must_use]
    pub fn one(project: Uuid) -> Self {
        AccessScope::Projects(Arc::new(HashSet::from([project])))
    }

    /// Whether this scope confines to a project set (vs. unrestricted).
    #[must_use]
    pub fn is_restricted(&self) -> bool {
        matches!(self, AccessScope::Projects(_))
    }

    /// Whether a given project is in scope.
    #[must_use]
    pub fn allows_project(&self, project: Uuid) -> bool {
        match self {
            AccessScope::Unrestricted => true,
            AccessScope::Projects(set) => set.contains(&project),
        }
    }

    /// The confined project ids, or `None` when unrestricted (no filtering).
    #[must_use]
    pub fn project_ids(&self) -> Option<Vec<Uuid>> {
        match self {
            AccessScope::Unrestricted => None,
            AccessScope::Projects(set) => Some(set.iter().copied().collect()),
        }
    }

    /// Like [`AccessScope::allows_project`] but for an entity whose project may be absent (a site
    /// with a NULL `project_id`). A restricted principal is denied a project-less resource; an
    /// unrestricted one sees everything.
    #[must_use]
    pub fn allows_project_opt(&self, project: Option<Uuid>) -> bool {
        match self {
            AccessScope::Unrestricted => true,
            AccessScope::Projects(set) => project.is_some_and(|p| set.contains(&p)),
        }
    }

    /// The confined project ids as a bindable Postgres `uuid[]` value for a raw `= ANY($n)` filter,
    /// or `None` when unrestricted (the caller then omits the filter). An empty set binds as an
    /// empty array, which `= ANY` treats as matching nothing, fail closed.
    #[must_use]
    pub fn sql_project_array(&self) -> Option<sea_orm::Value> {
        use sea_orm::sea_query::ArrayType;
        self.project_ids().map(|ids| {
            sea_orm::Value::Array(
                ArrayType::Uuid,
                Some(Box::new(ids.into_iter().map(sea_orm::Value::from).collect())),
            )
        })
    }
}

/// Keycloak realm roles, ordered by access level. Unrelated realm roles (`admin`, `offline_access`,
/// `uma_authorization`, …) land in `Unknown` and grant nothing.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Role {
    Intern,
    River,
    Manager,
    Administrator,
    Unknown(String),
}

/// Every realm role that admits a user, canonical first.
pub const RIVER_ROLE_NAMES: [&str; 4] = [
    "riverdata-admin",
    "riverdata-manager",
    "riverdata-river",
    "riverdata-intern",
];

impl Role {
    /// Ordered access level; 0 = no access. The comparison seam for the whole matrix.
    #[must_use]
    pub fn level(&self) -> u8 {
        match self {
            Role::Unknown(_) => 0,
            Role::Intern => 1,
            Role::River => 2,
            Role::Manager => 3,
            Role::Administrator => 4,
        }
    }

    /// Whether this realm role grants access to the application at all.
    #[must_use]
    pub fn grants_access(&self) -> bool {
        self.level() > 0
    }
}

impl axum_keycloak_auth::role::Role for Role {}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::Administrator => f.write_str("riverdata-admin"),
            Role::Manager => f.write_str("riverdata-manager"),
            Role::River => f.write_str("riverdata-river"),
            Role::Intern => f.write_str("riverdata-intern"),
            Role::Unknown(unknown) => f.write_fmt(format_args!("Unknown role: {unknown}")),
        }
    }
}

impl From<String> for Role {
    fn from(value: String) -> Self {
        match value.as_ref() {
            "riverdata-admin" => Role::Administrator,
            "riverdata-manager" => Role::Manager,
            "riverdata-river" => Role::River,
            "riverdata-intern" => Role::Intern,
            _ => Role::Unknown(value),
        }
    }
}

/// A single authorization capability. Both Keycloak roles and API-token permissions resolve into
/// a set of these; every route gate is expressed once as "requires capability X" instead of each
/// middleware re-deriving the role-vs-token policy by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    ReadMetadata,
    ReadData,
    /// Ingestion and data curation: readings/grab-sample writes, CSV import, flagging.
    WriteData,
    /// Field-level entity management: sites, site parameters, standard curves, notes, annotations.
    WriteFieldMetadata,
    /// Sensor movement: deployments, calibrations, adopt/swap/recall, reprocessing.
    ManageSensors,
    /// Global catalog: parameters, derived definitions, constants, alarm thresholds.
    WriteCatalog,
    /// Privileged management (tokens, sync credentials, user admin, sensor onboarding, streams).
    /// Granted only to the Keycloak Administrator role, never to an API token, by design.
    Admin,
}

impl Capability {
    /// THE policy table: the minimum realm-role level that holds each capability.
    #[must_use]
    pub fn min_role(&self) -> Role {
        match self {
            Capability::ReadMetadata | Capability::ReadData => Role::Intern,
            Capability::WriteData | Capability::WriteFieldMetadata => Role::River,
            Capability::ManageSensors | Capability::WriteCatalog => Role::Manager,
            Capability::Admin => Role::Administrator,
        }
    }

    /// The token permission bit that satisfies this capability when no explicit
    /// [`TokenAccess`] override is given. `None` = no token may hold it.
    #[must_use]
    pub fn default_token_bit(&self) -> Option<TokenBit> {
        match self {
            Capability::ReadMetadata => Some(TokenBit::ReadMetadata),
            Capability::ReadData => Some(TokenBit::ReadData),
            Capability::WriteData => Some(TokenBit::WriteData),
            // The historical `write_metadata` bit covered all entity management; the human-side
            // split does not change what a token may do.
            Capability::WriteFieldMetadata | Capability::ManageSensors | Capability::WriteCatalog => {
                Some(TokenBit::WriteMetadata)
            }
            Capability::Admin => None,
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Capability::ReadMetadata => "read_metadata",
            Capability::ReadData => "read_data",
            Capability::WriteData => "write_data",
            Capability::WriteFieldMetadata => "write_field_metadata",
            Capability::ManageSensors => "manage_sensors",
            Capability::WriteCatalog => "write_catalog",
            Capability::Admin => "admin",
        })
    }
}

/// The four API-token permission bits (frozen semantics, see `TokenPermissions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenBit {
    ReadMetadata,
    ReadData,
    WriteMetadata,
    WriteData,
}

/// How a route gate treats API tokens, independently of its Keycloak capability. Lets a route's
/// human-side level move (e.g. to Administrator) while the token bit that historically unlocked
/// it stays frozen, the sync microservices' session tokens must keep working unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenAccess {
    /// Token side follows the capability's [`Capability::default_token_bit`].
    Same,
    /// Token side requires this explicit bit regardless of the Keycloak capability.
    Bit(TokenBit),
    /// No token may pass, regardless of bits.
    Deny,
}

/// Structured permissions for API tokens.
/// Deserialized from the JSONB `permissions` column with serde defaults.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenPermissions {
    #[serde(default = "default_true")]
    pub read_metadata: bool,
    #[serde(default = "default_true")]
    pub read_data: bool,
    #[serde(default)]
    pub write_metadata: bool,
    #[serde(default)]
    pub write_data: bool,
}

fn default_true() -> bool {
    true
}

impl Default for TokenPermissions {
    fn default() -> Self {
        Self {
            read_metadata: true,
            read_data: true,
            write_metadata: false,
            write_data: false,
        }
    }
}

impl TokenPermissions {
    /// Parse from a `serde_json::Value`, falling back to defaults on any error.
    #[must_use]
    pub fn from_json(value: &serde_json::Value) -> Self {
        serde_json::from_value(value.clone()).unwrap_or_default()
    }

    #[must_use]
    pub fn has_bit(&self, bit: TokenBit) -> bool {
        match bit {
            TokenBit::ReadMetadata => self.read_metadata,
            TokenBit::ReadData => self.read_data,
            TokenBit::WriteMetadata => self.write_metadata,
            TokenBit::WriteData => self.write_data,
        }
    }
}

/// Decision function for Keycloak identities: the caller's highest role level must hold the
/// capability. Level 0 (no riverdata role) holds nothing.
#[must_use]
pub fn keycloak_allows(roles: &[Role], cap: Capability) -> bool {
    let level = roles.iter().map(Role::level).max().unwrap_or(0);
    level > 0 && level >= cap.min_role().level()
}

/// Decision function for API tokens under a gate's token rule.
#[must_use]
pub fn token_allows(permissions: &TokenPermissions, cap: Capability, rule: TokenAccess) -> bool {
    let bit = match rule {
        TokenAccess::Same => cap.default_token_bit(),
        TokenAccess::Bit(bit) => Some(bit),
        TokenAccess::Deny => None,
    };
    bit.is_some_and(|bit| permissions.has_bit(bit))
}

/// Shared gate body: 401 if unauthenticated, 403 if the identity lacks the capability (Keycloak)
/// or the token rule's bit (API token), else proceed. All `require_*` middlewares and the
/// declarative CRUD gates resolve here.
pub async fn check(
    cap: Capability,
    token_rule: TokenAccess,
    request: Request,
    next: Next,
) -> Response {
    use crate::common::middleware::AuthContext;
    match request.extensions().get::<AuthContext>() {
        Some(AuthContext::Keycloak { roles, .. }) if keycloak_allows(roles, cap) => {
            next.run(request).await
        }
        Some(AuthContext::ApiToken { permissions, .. })
            if token_allows(permissions, cap, token_rule) =>
        {
            next.run(request).await
        }
        Some(_) => AppError::Forbidden(format!("Requires {cap} capability")).into_response(),
        None => AppError::Unauthorized("Authentication required".to_string()).into_response(),
    }
}

/// Method-aware CRUD gate body: GET/HEAD need `read`, mutations need `write` (with `write_token`
/// governing the token side of mutations; reads follow the read capability's default bit).
pub async fn check_crud(
    read: Capability,
    write: Capability,
    write_token: TokenAccess,
    request: Request,
    next: Next,
) -> Response {
    let is_read = matches!(*request.method(), axum::http::Method::GET | axum::http::Method::HEAD);
    if is_read {
        check(read, TokenAccess::Same, request, next).await
    } else {
        check(write, write_token, request, next).await
    }
}
