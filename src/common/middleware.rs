use axum::{
    extract::{FromRequestParts, Request},
    http::{Method, request::Parts},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::collections::HashSet;
use std::sync::Arc;

use uuid::Uuid;

use crate::common::AppState;
use crate::common::authz::{self, AccessScope, Capability, Role, TokenAccess};
// Re-exported so `common::middleware::TokenPermissions` keeps resolving for existing call sites;
// the definition now lives with the rest of the policy in `authz`.
pub use crate::common::authz::TokenPermissions;
use crate::error::AppError;
use crate::routes::private::api_tokens::service::validate_bearer_token;

// Type alias for the Keycloak auth status used throughout this module.
type KcStatus =
    axum_keycloak_auth::KeycloakAuthStatus<Role, axum_keycloak_auth::decode::ProfileAndEmail>;

/// How the current request was authenticated.
#[derive(Debug, Clone)]
pub enum AuthContext {
    /// Authenticated via Keycloak JWT (admin UI, browser sessions).
    Keycloak {
        roles: Vec<Role>,
        /// The Keycloak `sub` (stable user id). Self-service notification endpoints bind strictly to
        /// this so a caller can only ever manage their own identity.
        sub: String,
        /// Best-effort user identity (email, else preferred_username) for audit fields.
        email: Option<String>,
        /// Whether a verified email claim is present (false when `email` is a username fallback).
        email_verified: bool,
        /// The projects this user is granted (from `user_project_grants`). Empty for a member with
        /// no grants (they see nothing, fail closed); ignored for administrators, who are
        /// unrestricted. Non-admin members flow through the same scope-filtering plumbing as
        /// project-scoped API tokens, generalized from one project to this set.
        grants: Arc<HashSet<Uuid>>,
    },
    /// Authenticated via an enrolled sync service's session token.
    ///
    /// It writes like a full-permission unscoped token, and unlike a token it says which source
    /// system it speaks for: what a service writes provenance under is a property of the identity
    /// it enrolled with, never a field in its requests.
    SyncService {
        service_id: Uuid,
        /// Declared on the credential the service enrolled with (M167). `None` on a credential
        /// minted before it was declared.
        source_system: Option<String>,
    },
    /// Authenticated via API token (external scripts, curl).
    ApiToken {
        token_id: Uuid,
        permissions: TokenPermissions,
        project_scope: Option<Uuid>,
        /// Per-token request ceiling (requests/second); `None` = unlimited.
        rate_limit_per_second: Option<i32>,
    },
}

impl AuthContext {
    pub fn has_role(&self, target: &Role) -> bool {
        match self {
            AuthContext::Keycloak { roles, .. } => roles.contains(target),
            AuthContext::ApiToken { .. } | AuthContext::SyncService { .. } => false,
        }
    }

    /// The caller's highest realm role, or `None` for an API token (which has bits, not a level).
    #[must_use]
    pub fn highest_role(&self) -> Option<Role> {
        match self {
            AuthContext::Keycloak { roles, .. } => roles.iter().max_by_key(|r| r.level()).cloned(),
            AuthContext::ApiToken { .. } | AuthContext::SyncService { .. } => None,
        }
    }

    pub fn is_admin(&self) -> bool {
        self.has_role(&Role::Administrator)
    }

    /// The Keycloak `sub` of the caller, if authenticated via Keycloak JWT. `None` for API tokens,
    /// self-service notification endpoints require a real user identity.
    pub fn keycloak_sub(&self) -> Option<&str> {
        match self {
            AuthContext::Keycloak { sub, .. } => Some(sub.as_str()),
            AuthContext::ApiToken { .. } | AuthContext::SyncService { .. } => None,
        }
    }

    /// The caller's email claim (Keycloak only), if present.
    pub fn email(&self) -> Option<&str> {
        match self {
            AuthContext::Keycloak { email, .. } => email.as_deref(),
            AuthContext::ApiToken { .. } | AuthContext::SyncService { .. } => None,
        }
    }

    /// Whether the caller has a verified email claim.
    pub fn email_verified(&self) -> bool {
        matches!(
            self,
            AuthContext::Keycloak {
                email_verified: true,
                ..
            }
        )
    }

    /// The projects this identity may see and act in. Sourced identically for every auth variant so
    /// scope-filtering is an identity-level concept: an unscoped token / sync token / Keycloak
    /// administrator is `Unrestricted`; a scoped token is confined to its one project; a non-admin
    /// Keycloak member is confined to their grant set.
    pub fn access_scope(&self) -> AccessScope {
        match self {
            AuthContext::ApiToken {
                project_scope: Some(p),
                ..
            } => AccessScope::one(*p),
            AuthContext::ApiToken {
                project_scope: None,
                ..
            }
            | AuthContext::SyncService { .. } => AccessScope::Unrestricted,
            AuthContext::Keycloak { roles, grants, .. } => {
                if roles.contains(&Role::Administrator) {
                    AccessScope::Unrestricted
                } else {
                    AccessScope::Projects(grants.clone())
                }
            }
        }
    }

    /// One name for this caller, as every trail records it: the email, else the Keycloak subject,
    /// else the source system a sync service speaks for, else the token or service it authenticated
    /// with.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            AuthContext::Keycloak { email: Some(e), .. } => e.clone(),
            AuthContext::Keycloak { sub, .. } => sub.clone(),
            AuthContext::SyncService {
                source_system: Some(system),
                ..
            } => format!("sync:{system}"),
            AuthContext::SyncService { service_id: id, .. } => format!("token:{id}"),
            AuthContext::ApiToken { token_id, .. } => format!("token:{token_id}"),
        }
    }

    /// What machinery this caller's writes are recorded as. A derivation names its own origin
    /// (janitor, chain, system) because no caller made it; everything a request writes is named
    /// here.
    #[must_use]
    pub fn origin(&self) -> crate::routes::private::readings::decisions::Origin {
        use crate::routes::private::readings::decisions::Origin;
        match self {
            AuthContext::SyncService { .. } => Origin::Sync,
            AuthContext::Keycloak { .. } | AuthContext::ApiToken { .. } => Origin::Manual,
        }
    }

    /// The source system this caller writes provenance under, for the register routes. `None` for
    /// a person or a token: only an enrolled service speaks for a source.
    #[must_use]
    pub fn source_system(&self) -> Option<&str> {
        match self {
            AuthContext::SyncService { source_system, .. } => source_system.as_deref(),
            AuthContext::Keycloak { .. } | AuthContext::ApiToken { .. } => None,
        }
    }

    /// The permission bits this identity carries on the token side of a gate, or `None` for a
    /// person, whose capabilities come from their role instead. An enrolled sync service carries a
    /// full unscoped set, stated once in [`TokenPermissions::sync_service`].
    #[must_use]
    pub fn token_permissions(&self) -> Option<TokenPermissions> {
        match self {
            AuthContext::ApiToken { permissions, .. } => Some(permissions.clone()),
            AuthContext::SyncService { .. } => Some(TokenPermissions::sync_service()),
            AuthContext::Keycloak { .. } => None,
        }
    }

    /// Whether this identity is granted a capability under the default token rule. Delegates to
    /// the policy in [`crate::common::authz`]: a Keycloak user's highest role level must hold the
    /// capability (level 0, no `riverdata-*` role, holds nothing, since the EPFL-federated realm
    /// makes authentication distinct from membership); an API token is limited to whichever of its
    /// four permission bits map to the capability and never holds `Admin`.
    pub fn allows(&self, cap: Capability) -> bool {
        match self {
            AuthContext::Keycloak { roles, .. } => authz::keycloak_allows(roles, cap),
            _ => self
                .token_permissions()
                .is_some_and(|p| authz::token_allows(&p, cap, TokenAccess::Same)),
        }
    }
}

/// Middleware that enables dual authentication: Keycloak JWT OR API token.
///
/// Runs after `KeycloakAuthLayer` in `PassthroughMode::Pass` mode.
/// Checks the Keycloak auth status first; if that failed, tries API token validation.
/// Inserts `AuthContext` into request extensions on success.
pub async fn service_auth_middleware(
    state: axum::extract::State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    // Check if Keycloak auth succeeded (inserted by KeycloakAuthLayer in Pass mode)
    if let Some(status) = request.extensions().get::<KcStatus>() {
        match status {
            axum_keycloak_auth::KeycloakAuthStatus::Success(token) => {
                let roles: Vec<Role> = token.roles.iter().map(|kr| kr.role().clone()).collect();
                // Access gate: a valid EPFL login is not membership. Reject role-less users here
                // with a distinct body (the UI keys on it) instead of falling through to token
                // auth, which would misreport an authenticated-but-unauthorized user as 401.
                if !roles.iter().any(Role::grants_access) {
                    tracing::info!(sub = %token.subject, "Keycloak login without a riverdata role rejected");
                    return AppError::Forbidden("no_river_role".to_string()).into_response();
                }
                let raw_email = token.extra.email.email.trim();
                // `email_verified` is only meaningful when a real email claim is present; the audit
                // `email` falls back to preferred_username, which is never a verified address.
                let email_verified = !raw_email.is_empty() && token.extra.email.email_verified;
                let email = if raw_email.is_empty() {
                    let u = token.extra.profile.preferred_username.trim();
                    (!u.is_empty()).then(|| u.to_string())
                } else {
                    Some(raw_email.to_string())
                };
                let sub = token.subject.clone();
                // Administrators are unrestricted, so skip the grant query entirely; every other
                // member is confined to their granted project set (loaded through a short-TTL cache).
                let grants = if roles.contains(&Role::Administrator) {
                    Arc::new(HashSet::new())
                } else {
                    crate::common::grants::load_grants(&state.db, &state.grants_cache, &sub).await
                };
                let auth = AuthContext::Keycloak {
                    roles,
                    sub,
                    email,
                    email_verified,
                    grants,
                };
                // The change-audit triggers read the writer from a transaction setting, which only
                // the request knows; every write this request makes runs inside this scope.
                let actor = crate::common::actor::label(&auth);
                request.extensions_mut().insert(auth);
                return crate::common::actor::scoped(actor, next.run(request)).await;
            }
            axum_keycloak_auth::KeycloakAuthStatus::Failure(_) => {
                // Keycloak auth failed, fall through to try API token
            }
        }
    }

    // Try API token auth from Authorization header
    let auth_header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    if let Some(header_value) = auth_header
        && let Some(token_model) =
            validate_bearer_token(&state.db, &header_value, &state.token_cache).await
    {
        let permissions = TokenPermissions::from_json(&token_model.permissions);
        // Per-token rate limit. `None`/`<=0` means unlimited, so tokens without a configured
        // ceiling (the default) are never throttled here.
        if let Some(rate) = token_model.rate_limit_per_second
            && rate > 0
            && !check_token_rate_limit(&state, token_model.id, rate).await
        {
            return AppError::TooManyRequests("Per-token rate limit exceeded".to_string())
                .into_response();
        }
        let token_id = token_model.id;
        let scope = token_model.project_scope;
        request.extensions_mut().insert(AuthContext::ApiToken {
            token_id,
            permissions,
            project_scope: scope,
            rate_limit_per_second: token_model.rate_limit_per_second,
        });
        // Capture request shape before consuming it, then record the outcome (incl. any 403 the
        // token earned) to the forensic audit log when enabled. Fire-and-forget; never blocks.
        let audit = state.config.audit_api_token_use;
        let method = request.method().as_str().to_string();
        let path = request.uri().path().to_string();
        let response =
            crate::common::actor::scoped(format!("token:{token_id}"), next.run(request)).await;
        if audit {
            crate::routes::private::api_tokens::service::record_token_use(
                &state.db,
                token_id,
                scope,
                &method,
                &path,
                response.status().as_u16(),
            );
        }
        return response;
    }

    // Try sync service session token as last resort.
    // The sync microservice authenticates via /api/sync/enroll but then
    // needs to call regular service-tier endpoints (streams, ingest, readings/batch, etc.).
    let sync_header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|v| v.trim());

    if let Some(raw) = sync_header
        && !raw.is_empty()
    {
        // Same resolution the control plane extractor uses, so the expiry rule cannot drift
        // between the two surfaces a session token reaches.
        if let Some(session) =
            crate::routes::private::sync::control::session::lookup_sync_session(&state.db, raw)
                .await
        {
            let auth = AuthContext::SyncService {
                service_id: session.service_id,
                source_system: session.source_system,
            };
            let actor = auth.label();
            request.extensions_mut().insert(auth);
            return crate::common::actor::scoped(actor, next.run(request)).await;
        }
    }

    // No auth method succeeded
    AppError::Unauthorized("Valid Keycloak JWT or API token required".to_string()).into_response()
}

/// Requires the `read_metadata` capability (any member; token with read_metadata).
pub async fn require_read_metadata(request: Request, next: Next) -> Response {
    authz::check(Capability::ReadMetadata, TokenAccess::Same, request, next).await
}

/// Requires the `read_data` capability.
pub async fn require_read_data(request: Request, next: Next) -> Response {
    authz::check(Capability::ReadData, TokenAccess::Same, request, next).await
}

/// Requires the `write_data` capability (RIVER member; token with write_data).
pub async fn require_write_data(request: Request, next: Next) -> Response {
    authz::check(Capability::WriteData, TokenAccess::Same, request, next).await
}

/// Requires the `enter_field_data` capability: any member down to intern, or a token with
/// `write_data`. An intern's entry lands unverified and cannot displace a stored value; the
/// handler enforces both.
pub async fn require_enter_field_data(request: Request, next: Next) -> Response {
    authz::check(Capability::EnterFieldData, TokenAccess::Same, request, next).await
}

/// Requires the `manage_sensors` capability (MANAGER member; token with write_metadata).
pub async fn require_manage_sensors(request: Request, next: Next) -> Response {
    authz::check(Capability::ManageSensors, TokenAccess::Same, request, next).await
}

/// Requires the `Admin` capability, the Keycloak Administrator role only. NO API token can pass
/// (tokens never hold `Admin`): defense in depth for user management, token mutation, and sync
/// credential creation.
pub async fn require_admin(request: Request, next: Next) -> Response {
    authz::check(Capability::Admin, TokenAccess::Deny, request, next).await
}

/// Keycloak Administrator OR an API token carrying `write_metadata`. For routes that are
/// human-Administrator-only (streams register/pair, sensor onboarding, jobs) yet are legitimately
/// driven by sync-service session tokens, which hold `write_metadata`. Keeps the human RBAC strict
/// without breaking the microservices.
pub async fn require_admin_or_token_write_metadata(request: Request, next: Next) -> Response {
    authz::check(
        Capability::Admin,
        TokenAccess::Bit(authz::TokenBit::WriteMetadata),
        request,
        next,
    )
    .await
}

/// Method-aware `CrudCrate` gate: GET/HEAD need `read`, mutations need `write` (with `write_token`
/// governing the token side of mutations). Returned as a closure so the per-entity capabilities
/// are captured at wiring time in `service/mod.rs`.
pub fn require_crud(
    read: Capability,
    write: Capability,
    write_token: TokenAccess,
) -> impl Fn(Request, Next) -> futures::future::BoxFuture<'static, Response> + Clone {
    move |request, next| Box::pin(authz::check_crud(read, write, write_token, request, next))
}

/// Extractor: true when the caller is an authenticated sync service.
pub struct IsSyncService(pub bool);

impl<S: Send + Sync> FromRequestParts<S> for IsSyncService {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(IsSyncService(matches!(
            parts.extensions.get::<AuthContext>(),
            Some(AuthContext::SyncService { .. })
        )))
    }
}

/// Extractor yielding the caller's [`AccessScope`]: `Unrestricted` for an administrator, an
/// unscoped token and a sync token; the granted project set for a member; the single project of a
/// scoped token.
///
/// Fails closed. Every route carrying this extractor sits behind [`service_auth_middleware`], which
/// inserts an [`AuthContext`] or rejects, so a missing context means the route was wired outside the
/// authenticated surface. Defaulting that to `Unrestricted` (the previous behavior) turned a wiring
/// mistake into a silent cross-project read.
#[derive(Debug, Clone)]
pub struct ProjectScope(pub AccessScope);

impl<S: Send + Sync> FromRequestParts<S> for ProjectScope {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let scope = parts
            .extensions
            .get::<AuthContext>()
            .map(AuthContext::access_scope)
            .ok_or_else(|| {
                AppError::Unauthorized("Valid Keycloak JWT or API token required".to_string())
                    .into_response()
            })?;
        Ok(ProjectScope(scope))
    }
}

/// Extractor that rejects any project-scoped principal with 403. The reject-on-read counterpart of
/// the `deny_scoped_token` middleware: used inside operator/analysis read handlers (cross-project
/// candidate enumeration, etc.) whose write counterparts are already behind `deny_scoped_token`, so a
/// per-client logger key can neither trigger the action nor enumerate the inventory feeding it.
/// Keycloak users and unscoped tokens pass; sources scope identity-level (future scoped Keycloak
/// users are denied too).
pub struct DenyScoped;

impl<S: Send + Sync> FromRequestParts<S> for DenyScoped {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // Deny scoped API tokens only, a granted Keycloak member is confined by their grant set,
        // not blocked outright from operator actions their capability admits.
        let scoped_token = matches!(
            parts.extensions.get::<AuthContext>(),
            Some(AuthContext::ApiToken {
                project_scope: Some(_),
                ..
            })
        );
        if scoped_token {
            return Err(AppError::Forbidden(
                "Project-scoped tokens cannot call operator or cross-project actions".to_string(),
            )
            .into_response());
        }
        Ok(DenyScoped)
    }
}

/// Per-token rate-limit check using a direct governor limiter keyed by token id. Returns `true`
/// if the request is allowed. Builds (or rebuilds, if the configured rate changed) the limiter on
/// first use. `rate <= 0` is treated as unlimited. The registry is a bounded moka cache, so it
/// can't grow without limit and idle keys are evicted.
async fn check_token_rate_limit(state: &AppState, token_id: Uuid, rate: i32) -> bool {
    use governor::{Quota, RateLimiter};
    let Some(per_sec) = u32::try_from(rate).ok().and_then(std::num::NonZeroU32::new) else {
        return true;
    };
    let limiter = match state.token_rate_limiters.get(&token_id).await {
        Some((existing_rate, limiter)) if existing_rate == rate => limiter,
        _ => {
            let limiter = std::sync::Arc::new(RateLimiter::direct(Quota::per_second(per_sec)));
            state
                .token_rate_limiters
                .insert(token_id, (rate, limiter.clone()))
                .await;
            limiter
        }
    };
    limiter.check().is_ok()
}

/// Invalidate the API-token validation cache after any successful mutating request on the `/tokens`
/// router. The explicit `revoke`/`rotate` handlers bust the cache themselves; this covers the
/// CrudCrate-generated DELETE and PATCH (e.g. setting `is_active = false`), so a disabled or deleted
/// token stops authenticating on the very next request instead of lingering for the cache TTL.
pub async fn bust_token_cache_on_mutation(
    state: axum::extract::State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let mutating = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    let response = next.run(request).await;
    if mutating && response.status().is_success() {
        crate::routes::private::api_tokens::service::invalidate_token_cache(&state.token_cache)
            .await;
    }
    response
}

/// Deny project-scoped API tokens outright. Layered on operator/global action routes (sensor
/// adopt/swap, stream management, reprocess/backfill/aggregate refresh, merges, recalculate) that
/// either span projects or have no per-project target, work a per-client logger key has no reason
/// to do. Keycloak users and unscoped API tokens pass through unchanged.
pub async fn deny_scoped_token(request: Request, next: Next) -> Response {
    if let Some(AuthContext::ApiToken {
        project_scope: Some(_),
        ..
    }) = request.extensions().get::<AuthContext>()
    {
        return AppError::Forbidden(
            "Project-scoped tokens cannot call operator or cross-project actions; use an unscoped \
             token or the admin UI"
                .to_string(),
        )
        .into_response();
    }
    next.run(request).await
}

/// Reject the request if a restricted principal is writing to any site outside its scope. No-op for
/// unrestricted callers (administrators, unscoped/sync tokens). `site_ids` are the distinct sites
/// the request would touch; an unknown site is also rejected. Applies uniformly to a project-scoped
/// API token and to a non-admin Keycloak member (whose scope is their granted project set).
pub async fn enforce_project_scope_for_sites(
    db: &sea_orm::DatabaseConnection,
    scope: &AccessScope,
    site_ids: &[Uuid],
) -> Result<(), AppError> {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    let AccessScope::Projects(_) = scope else {
        return Ok(());
    };
    let mut seen = std::collections::HashSet::new();
    for site_id in site_ids {
        if !seen.insert(*site_id) {
            continue;
        }
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT project_id FROM sites WHERE id = $1",
                [(*site_id).into()],
            ))
            .await
            .map_err(AppError::Database)?;
        let in_scope = match row {
            Some(r) => r
                .try_get::<Option<Uuid>>("", "project_id")
                .ok()
                .flatten()
                .is_some_and(|pid| scope.allows_project(pid)),
            None => false,
        };
        if !in_scope {
            return Err(AppError::Forbidden(
                "Site is outside your project access".to_string(),
            ));
        }
    }
    Ok(())
}

/// Read-side scope filter: the site ids belonging to a restricted principal's project set, or `None`
/// when unrestricted (no filtering). Handlers that return rows keyed by `site_id` pass the returned
/// list as `site_id = ANY($n)`. An empty `Some(vec![])` (a member whose granted projects have no
/// sites, or a member with no grants) correctly filters everything out.
pub async fn scope_site_ids(
    db: &sea_orm::DatabaseConnection,
    scope: &AccessScope,
) -> Result<Option<Vec<Uuid>>, AppError> {
    use crate::routes::private::sites;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QuerySelect};
    let Some(project_ids) = scope.project_ids() else {
        return Ok(None);
    };
    let ids = sites::Entity::find()
        .select_only()
        .column(sites::Column::Id)
        .filter(sites::Column::ProjectId.is_in(project_ids))
        .into_tuple::<Uuid>()
        .all(db)
        .await
        .map_err(AppError::Database)?;
    Ok(Some(ids))
}

/// Whether a sensor is visible to a restricted principal: `true` if unrestricted, otherwise `true`
/// only when the sensor has at least one deployment to a site within the scoped project set.
/// Single-resource sensor read endpoints use this to 404 a cross-scope sensor.
///
/// One resolver: the projects come from [`crate::common::scope::project_of_sensor`], the same one
/// the id-addressed action guards use, so a sensor cannot be visible on one surface and invisible on
/// another. A sensor with no deployment resolves to no project and is denied here; an action that
/// deliberately admits uncommitted inventory passes [`crate::common::scope::Unowned::Allow`] instead.
pub async fn sensor_in_scope(
    db: &sea_orm::DatabaseConnection,
    scope: &AccessScope,
    sensor_id: Uuid,
) -> Result<bool, AppError> {
    use crate::common::scope::{Unowned, project_of_sensor, require_row_in_scope};
    let row = project_of_sensor(db, sensor_id).await?;
    Ok(require_row_in_scope(scope, &row, Unowned::Deny, "sensor").is_ok())
}

/// Subquery selecting the ids of the sites in a restricted principal's project set. Used to confine
/// child entities whose own scoping column is `site_id` (`notes`, `annotations`, …) without an extra
/// round-trip, it inlines as a SQL sub-select in the read filter.
fn scoped_site_ids_query(projects: &[Uuid]) -> sea_orm::sea_query::SelectStatement {
    use crate::routes::private::sites;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QuerySelect, QueryTrait};
    sites::Entity::find()
        .select_only()
        .column(sites::Column::Id)
        .filter(sites::Column::ProjectId.is_in(projects.iter().copied()))
        .into_query()
}

/// Subquery selecting sensor ids that have at least one deployment at a site in the scoped set.
/// Used to confine `sensors` and `sensor_calibrations`.
fn scoped_sensor_ids_query(projects: &[Uuid]) -> sea_orm::sea_query::SelectStatement {
    use crate::routes::private::sensors::deployments;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QuerySelect, QueryTrait};
    deployments::Entity::find()
        .select_only()
        .column(deployments::Column::SensorId)
        .filter(deployments::Column::SiteId.in_subquery(scoped_site_ids_query(projects)))
        .into_query()
}

/// Subquery selecting the site_parameter ids within a restricted principal's project set. Used to
/// confine `data_streams`, whose scoping column is `site_parameter_id`.
fn scoped_site_parameter_ids_query(projects: &[Uuid]) -> sea_orm::sea_query::SelectStatement {
    use crate::routes::private::sites::parameters as site_parameters;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QuerySelect, QueryTrait};
    site_parameters::Entity::find()
        .select_only()
        .column(site_parameters::Column::Id)
        .filter(site_parameters::Column::SiteId.in_subquery(scoped_site_ids_query(projects)))
        .into_query()
}

/// Which side of a CRUD route [`crud_scope_condition`] is answering for. Almost every entity gives
/// one answer to all three; the exceptions are the rows whose project is derived from where an
/// instrument has been deployed, and they are named in the match.
///
/// A lab instrument is bench equipment shared across projects and is never deployed, so "which
/// projects does this row belong to" answers "none" for the inventory itself and for a curve
/// measured on it. Reading such a row is confined (it is listed only where the instrument stood),
/// but refusing to *write* one would make the shared inventory unmaintainable by the very people
/// who keep it, so a member's write treats an unbound row as having no project boundary to cross.
/// A project-scoped API token is refused it either way: an unbound row is shared metadata, and a
/// per-client key never writes that.
#[derive(Clone, Copy, PartialEq)]
enum Direction {
    /// A list or a get.
    Read,
    /// A write by a non-admin Keycloak member, confined to their granted projects.
    MemberWrite,
    /// A write by a project-scoped API token.
    TokenWrite,
}

/// Subquery selecting every sensor id that has any deployment at all. Its complement is the bench
/// inventory, which belongs to no project.
fn deployed_sensor_ids_query() -> sea_orm::sea_query::SelectStatement {
    use crate::routes::private::sensors::deployments;
    use sea_orm::{EntityTrait, QuerySelect, QueryTrait};
    deployments::Entity::find()
        .select_only()
        .column(deployments::Column::SensorId)
        .into_query()
}

/// Row-filter confining a CRUD entity to a restricted principal's project set, or `None` for
/// global-catalog, operational, and admin-only entities (reading shared definitions like
/// `parameters`/`constants` is intended). Built as a subquery so it adds no round-trip and
/// references only the entity's own columns; rows whose scoping column is NULL (unpaired streams,
/// site-less global thresholds) fall out by construction.
///
/// This is the only statement of the rule. It confines reads and writes alike: crudcrate filters a
/// list by it, 404s a get or an update or a delete it excludes, and refuses a create or an update
/// whose resulting row falls outside it.
///
/// `direction` is where the read and the write rules part, which they do only for the rows whose
/// project comes from an instrument's deployments; see [`Direction`].
fn crud_scope_condition(
    entity: &str,
    projects: &[Uuid],
    direction: Direction,
) -> Option<sea_orm::Condition> {
    use crate::routes::private::{
        alarms::thresholds, annotations, data_streams, notes, projects as projects_entity,
        projects::subprojects, readings::samples, reprocessing_jobs, sensors,
        sensors::calibrations, sensors::deployments, sensors::standard_curves, sites,
        sites::parameters as site_parameters,
    };
    use sea_orm::{ColumnTrait, Condition};
    let ids = || projects.iter().copied();
    let expr = match entity {
        // Writing a project is an administrative act over the container itself, not a write
        // inside it, so it carries no project dimension to be confined by. Reads are confined to
        // the granted set.
        "projects" if direction != Direction::Read => return None,
        "projects" => projects_entity::Column::Id.is_in(ids()),
        "subprojects" => subprojects::Column::ProjectId.is_in(ids()),
        "sites" => sites::Column::ProjectId.is_in(ids()),
        "site_parameters" => {
            site_parameters::Column::SiteId.in_subquery(scoped_site_ids_query(projects))
        }
        "notes" => notes::Column::SiteId.in_subquery(scoped_site_ids_query(projects)),
        "annotations" => annotations::Column::SiteId.in_subquery(scoped_site_ids_query(projects)),
        "sensor_deployments" => {
            deployments::Column::SiteId.in_subquery(scoped_site_ids_query(projects))
        }
        "alarm_thresholds" => {
            thresholds::Column::SiteId.in_subquery(scoped_site_ids_query(projects))
        }
        "samples" => samples::Column::SiteId.in_subquery(scoped_site_ids_query(projects)),
        "data_streams" => data_streams::Column::SiteParameterId
            .in_subquery(scoped_site_parameter_ids_query(projects)),
        // The instrument inventory is shared: a sensor row carries no project of its own, and a
        // sensor being added has stood nowhere yet. Reads are confined to where it has been
        // deployed; writes are catalog writes, governed by the caller's role and refused to a
        // project-scoped token by `inject_project_scope`.
        "sensors" if direction != Direction::Read => return None,
        "sensors" => sensors::Column::Id.in_subquery(scoped_sensor_ids_query(projects)),
        "sensor_calibrations" => {
            calibrations::Column::SensorId.in_subquery(scoped_sensor_ids_query(projects))
        }
        "standard_curves" => {
            let scoped =
                standard_curves::Column::SensorId.in_subquery(scoped_sensor_ids_query(projects));
            return Some(if direction == Direction::MemberWrite {
                Condition::any().add(scoped).add(
                    standard_curves::Column::SensorId.not_in_subquery(deployed_sensor_ids_query()),
                )
            } else {
                Condition::all().add(scoped)
            });
        }
        // Mirrors `scope::project_of_job`: a job belongs to its site, else to the projects its
        // sensor is deployed into, and a job targeting neither is global. Returning early here
        // would hide global jobs (a CSV import targets no sensor) from the member who started one.
        // A job is an operational row, not a project's. Writing one is administrative, and a
        // global job (a CSV import targets no sensor) belongs to no project to be confined by.
        "reprocessing_jobs" if direction != Direction::Read => return None,
        "reprocessing_jobs" => {
            return Some(
                Condition::any()
                    .add(
                        reprocessing_jobs::Column::SiteId
                            .in_subquery(scoped_site_ids_query(projects)),
                    )
                    .add(
                        reprocessing_jobs::Column::SensorId
                            .in_subquery(scoped_sensor_ids_query(projects)),
                    )
                    .add(
                        Condition::all()
                            .add(reprocessing_jobs::Column::SiteId.is_null())
                            .add(reprocessing_jobs::Column::SensorId.is_null()),
                    ),
            );
        }
        _ => return None,
    };
    Some(Condition::all().add(expr))
}

/// Project-scope confinement for the CRUD entity routers. For a restricted principal (a scoped API
/// token, or a non-admin Keycloak member confined to their grant set) injects a CrudCrate
/// [`crudcrate::ScopeCondition`], which confines every generated handler: a list is filtered to the
/// project set, a get, update or delete of an excluded row is a 404, and a create or update whose
/// resulting row would fall outside it is refused inside the write's own transaction. No-op for
/// unrestricted principals. Custom sub-routes (e.g. `/sites/{id}/readings`) don't read the
/// extension and keep their own manual scope checks.
///
/// The entities [`crud_scope_condition`] answers `None` for have no project dimension: the shared
/// catalog, the operational tables, a schedule. A **Keycloak member** may write those (their role
/// capability already governs shared metadata, and catalog writes are Administrator-only anyway); a
/// **project-scoped API token** may not, and is refused here, since a per-client key has no business
/// mutating what every project reads. That refusal is the one rule a row filter cannot express,
/// because there is no row column to filter on.
pub async fn inject_project_scope(request: Request, next: Next) -> Response {
    let Some(auth) = request.extensions().get::<AuthContext>() else {
        return next.run(request).await;
    };
    let is_token = matches!(
        auth,
        AuthContext::ApiToken { .. } | AuthContext::SyncService { .. }
    );
    let Some(project_ids) = auth.access_scope().project_ids() else {
        return next.run(request).await;
    };
    let Some(entity) = crud_entity(request.uri().path()) else {
        return next.run(request).await;
    };
    let writing = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    let direction = match (writing, is_token) {
        (false, _) => Direction::Read,
        (true, false) => Direction::MemberWrite,
        (true, true) => Direction::TokenWrite,
    };
    match crud_scope_condition(entity, &project_ids, direction) {
        Some(condition) => {
            let mut request = request;
            request
                .extensions_mut()
                .insert(crudcrate::ScopeCondition::new(condition));
            next.run(request).await
        }
        None if writing && is_token => AppError::Forbidden(format!(
            "Project-scoped token cannot modify '{entity}'"
        ))
        .into_response(),
        None => next.run(request).await,
    }
}

/// The entity segment of a CRUD path like `/api/site_parameters/{id}`, which is what names the row
/// filter. The segments after it are the handler's business.
fn crud_entity(path: &str) -> Option<&str> {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let start = segs.iter().position(|s| *s == "api").map_or(0, |i| i + 1);
    segs.get(start).copied()
}

#[cfg(test)]
mod tests {
    use super::{Direction, crud_entity, crud_scope_condition};
    use uuid::Uuid;

    #[test]
    fn test_crud_entity_reads_the_entity_and_ignores_what_follows_it() {
        assert_eq!(crud_entity("/api/site_parameters"), Some("site_parameters"));
        assert_eq!(
            crud_entity("/api/site_parameters/batch"),
            Some("site_parameters"),
            "`batch` is a sub-route of the entity, not another entity"
        );
        assert_eq!(
            crud_entity("/api/site_parameters/0189d3f0-0000-4000-8000-000000000000"),
            Some("site_parameters")
        );
        assert_eq!(crud_entity("/"), None);
    }

    /// An entity answering `None` has no project dimension, which is what makes
    /// `inject_project_scope` refuse a scoped token's write to it. The refusal is the one rule no
    /// row filter can state, so the set it applies to is asserted here.
    #[test]
    fn test_the_entities_with_no_project_dimension_are_the_ones_a_scoped_token_is_refused() {
        let projects = [Uuid::nil()];
        let none_for = |direction| {
            let mut names: Vec<&str> = Vec::new();
            for entity in [
                "projects", "sites", "site_parameters", "notes", "annotations", "samples",
                "subprojects", "data_streams", "alarm_thresholds", "sensor_deployments",
                "sensor_calibrations", "standard_curves", "sensors", "reprocessing_jobs",
                "parameters", "constants", "schedules", "collection_events",
            ] {
                if crud_scope_condition(entity, &projects, direction).is_none() {
                    names.push(entity);
                }
            }
            names
        };
        assert_eq!(
            none_for(Direction::Read),
            ["parameters", "constants", "schedules", "collection_events"],
            "a read is confined for every entity whose rows carry a project"
        );
        let expected = [
            "projects",
            "sensors",
            "reprocessing_jobs",
            "parameters",
            "constants",
            "schedules",
            "collection_events",
        ];
        for direction in [Direction::MemberWrite, Direction::TokenWrite] {
            let mut got = none_for(direction);
            got.sort_unstable();
            let mut want = expected;
            want.sort_unstable();
            assert_eq!(got, want, "the shared inventory is written as catalog, not as a project's");
        }
    }

    /// The bench instrument is the one row a member may write and may not read, so the asymmetry is
    /// asserted rather than described.
    #[test]
    fn test_only_a_members_write_reaches_a_curve_on_an_instrument_deployed_nowhere() {
        let projects = [Uuid::nil()];
        let sql = |entity: &str, direction| {
            format!(
                "{:?}",
                crud_scope_condition(entity, &projects, direction).expect("a project-bound entity")
            )
        };
        assert_ne!(
            sql("standard_curves", Direction::Read),
            sql("standard_curves", Direction::MemberWrite),
            "a member's write must reach a curve on an instrument deployed nowhere"
        );
        assert_eq!(
            sql("standard_curves", Direction::Read),
            sql("standard_curves", Direction::TokenWrite),
            "a project-scoped token is confined to where the instrument stood, in both directions"
        );
        for entity in ["sites", "notes", "sensor_calibrations", "data_streams"] {
            assert_eq!(
                sql(entity, Direction::Read),
                sql(entity, Direction::MemberWrite),
                "{entity} has one rule in every direction"
            );
        }
    }
}
