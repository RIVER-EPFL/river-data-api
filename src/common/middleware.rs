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
            AuthContext::ApiToken { .. } => false,
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
            AuthContext::ApiToken { .. } => None,
        }
    }

    /// The caller's email claim (Keycloak only), if present.
    pub fn email(&self) -> Option<&str> {
        match self {
            AuthContext::Keycloak { email, .. } => email.as_deref(),
            AuthContext::ApiToken { .. } => None,
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
            } => AccessScope::Unrestricted,
            AuthContext::Keycloak { roles, grants, .. } => {
                if roles.contains(&Role::Administrator) {
                    AccessScope::Unrestricted
                } else {
                    AccessScope::Projects(grants.clone())
                }
            }
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
            AuthContext::ApiToken { permissions, .. } => {
                authz::token_allows(permissions, cap, TokenAccess::Same)
            }
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
                request.extensions_mut().insert(AuthContext::Keycloak {
                    roles,
                    sub,
                    email,
                    email_verified,
                    grants,
                });
                return next.run(request).await;
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
        let response = next.run(request).await;
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
        if let Some(token) =
            crate::routes::private::sync::control::session::lookup_sync_session(&state.db, raw)
                .await
        {
            request.extensions_mut().insert(AuthContext::ApiToken {
                token_id: token.service_id,
                permissions: TokenPermissions {
                    read_metadata: true,
                    read_data: true,
                    write_metadata: true,
                    write_data: true,
                },
                project_scope: None,
                rate_limit_per_second: None,
            });
            request.extensions_mut().insert(SyncServiceMarker);
            return next.run(request).await;
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

/// Marker inserted by the dual-auth middleware when the caller authenticated with a sync
/// service session token, for handlers whose behavior differs for sync services (ie. the
/// ingest `overwrite` flag).
#[derive(Clone, Copy)]
pub struct SyncServiceMarker;

/// Extractor: true when the caller is an authenticated sync service.
pub struct IsSyncService(pub bool);

impl<S: Send + Sync> FromRequestParts<S> for IsSyncService {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(IsSyncService(
            parts.extensions.get::<SyncServiceMarker>().is_some(),
        ))
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

/// Mutating-CRUD scope guard, layered on the entity router. For a restricted principal performing a
/// create/update/delete, resolves the target row's owning project and rejects anything outside the
/// principal's scope. Applies to both a project-scoped API token and a non-admin Keycloak member
/// (confined to their granted project set). Unrestricted principals (administrators, unscoped/sync
/// tokens) pass through untouched (and pay no body-buffering cost).
///
/// The global catalog (`parameters`, `sensors`, `constants`, …) has no owning project. A **scoped
/// API token** is denied it (fail closed, a per-client key can never mutate shared metadata). A
/// **Keycloak member** is allowed through: their capability gate already decides whether they may
/// write it (e.g. catalog writes are Administrator-only, so a non-admin member never reaches those
/// routes anyway), and global catalog entities are legitimately managed by members.
pub async fn enforce_scope_on_crud(
    state: axum::extract::State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let (scope, is_token) = match request.extensions().get::<AuthContext>() {
        Some(ctx @ AuthContext::ApiToken { .. }) => (ctx.access_scope(), true),
        Some(ctx @ AuthContext::Keycloak { .. }) => (ctx.access_scope(), false),
        None => return next.run(request).await,
    };
    if !scope.is_restricted() {
        return next.run(request).await;
    }

    if matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        return next.run(request).await;
    }

    let path = request.uri().path().to_string();
    let Some((entity, id)) = parse_crud_target(&path) else {
        return AppError::Forbidden("You cannot perform this operation".to_string())
            .into_response();
    };
    let entity = entity.to_string();
    let id = id.map(str::to_string);

    // Buffer the body so a create payload can be inspected and then forwarded intact.
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, 4 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return AppError::BadRequest("Request body too large".to_string()).into_response();
        }
    };
    let json: Option<serde_json::Value> = serde_json::from_slice(&bytes).ok();

    let outside = || {
        AppError::Forbidden("That resource is outside your project access".to_string())
            .into_response()
    };
    match resolve_scope_project(&state.db, &entity, id.as_deref(), json.as_ref()).await {
        // Every owning/target project must be in scope (a single owner, or owner + repoint target).
        ScopeOutcome::RequireAll(projects) => {
            if projects.is_empty() || !projects.iter().all(|p| scope.allows_project(*p)) {
                return outside();
            }
        }
        // At least one must be in scope, matches read confinement for a multi-project entity (a
        // calibration is visible/writable if its sensor touches a granted project). Empty fails closed.
        ScopeOutcome::RequireAny(projects) => {
            if !projects.iter().any(|p| scope.allows_project(*p)) {
                return outside();
            }
        }
        // A project-scoped entity whose owning project couldn't be resolved (row missing/unbound, or a
        // create that omits the owning FK to dodge the check): fail closed for any restricted principal.
        // Administrators never reach here, the unrestricted early-return above skips this whole guard.
        ScopeOutcome::Unresolved(msg) => {
            return AppError::Forbidden(msg).into_response();
        }
        // An entity with no project dimension (global catalog: parameters, constants, sensors, …):
        // a project-scoped token may not touch it; a member may (their role capability governs shared
        // metadata writes).
        ScopeOutcome::Global(msg) => {
            if is_token {
                return AppError::Forbidden(msg).into_response();
            }
        }
    }

    let request = Request::from_parts(parts, axum::body::Body::from(bytes));
    next.run(request).await
}

enum ScopeOutcome {
    /// All listed projects must be in the caller's scope (owning project + any repoint target).
    RequireAll(Vec<Uuid>),
    /// At least one listed project must be in scope (a multi-project entity). Empty → fail closed.
    RequireAny(Vec<Uuid>),
    /// Project-scoped but the owning project couldn't be resolved → fail closed for restricted callers.
    Unresolved(String),
    /// No project dimension (global catalog) → members allowed (role governs), tokens denied.
    Global(String),
}

/// Extract `(entity, optional id)` from a CRUD path like `/api/site_parameters/{id}`.
fn parse_crud_target(path: &str) -> Option<(&str, Option<&str>)> {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let start = segs.iter().position(|s| *s == "api").map_or(0, |i| i + 1);
    let entity = segs.get(start)?;
    let id = segs.get(start + 1).copied();
    Some((entity, id))
}

async fn resolve_scope_project(
    db: &sea_orm::DatabaseConnection,
    entity: &str,
    id: Option<&str>,
    body: Option<&serde_json::Value>,
) -> ScopeOutcome {
    let fk = |key: &str| {
        body.and_then(|b| b.get(key))
            .and_then(serde_json::Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
    };

    // Update / delete: resolve the owning project(s) from the existing row.
    if let Some(id) = id {
        let Ok(uuid) = Uuid::parse_str(id) else {
            return ScopeOutcome::Unresolved("Could not resolve target".to_string());
        };
        // sensor_calibrations spans every project its sensor is deployed to; matches read scoping
        // (visible/writable if the sensor touches a granted project), so require ANY.
        if entity == "sensor_calibrations" {
            let projects = distinct_projects(
                db,
                "SELECT DISTINCT s.project_id FROM sensor_calibrations c \
                 JOIN sensor_deployments d ON d.sensor_id = c.sensor_id \
                 JOIN sites s ON s.id = d.site_id WHERE c.id = $1",
                uuid,
            )
            .await;
            return require_any(projects, "calibration");
        }
        // A standard curve belongs to an instrument, so it spans the project set that instrument
        // has been deployed to. A bench instrument is deployed nowhere, which is why the empty
        // resolution reads as no project dimension rather than as a denial; see
        // [`require_any_or_unbound`].
        if entity == "standard_curves" {
            let projects = distinct_projects(
                db,
                "SELECT DISTINCT s.project_id FROM standard_curves c \
                 JOIN sensor_deployments d ON d.sensor_id = c.sensor_id \
                 JOIN sites s ON s.id = d.site_id WHERE c.id = $1",
                uuid,
            )
            .await;
            return require_any_or_unbound(projects, "standard curve");
        }
        let sql = match entity {
            "sites" => "SELECT project_id FROM sites WHERE id = $1",
            "subprojects" => "SELECT project_id FROM subprojects WHERE id = $1",
            "site_parameters" => {
                "SELECT s.project_id FROM site_parameters sp JOIN sites s ON s.id = sp.site_id WHERE sp.id = $1"
            }
            "notes" => {
                "SELECT s.project_id FROM notes n JOIN sites s ON s.id = n.site_id WHERE n.id = $1"
            }
            "annotations" => {
                "SELECT s.project_id FROM annotations a JOIN sites s ON s.id = a.site_id WHERE a.id = $1"
            }
            "sensor_deployments" => {
                "SELECT s.project_id FROM sensor_deployments d JOIN sites s ON s.id = d.site_id WHERE d.id = $1"
            }
            "alarm_thresholds" => {
                "SELECT s.project_id FROM alarm_thresholds t JOIN sites s ON s.id = t.site_id WHERE t.id = $1"
            }
            "samples" => {
                "SELECT s.project_id FROM samples sm JOIN sites s ON s.id = sm.site_id WHERE sm.id = $1"
            }
            "data_streams" => {
                "SELECT s.project_id FROM data_streams ds JOIN site_parameters sp ON sp.id = ds.site_parameter_id JOIN sites s ON s.id = sp.site_id WHERE ds.id = $1"
            }
            other => {
                return ScopeOutcome::Global(format!(
                    "Project-scoped token cannot modify '{other}'"
                ));
            }
        };
        // The owning project of the existing row, plus any repoint target in the update body: moving a
        // site to another subproject, or a subproject to another project, must land in a scope the
        // caller also holds. All resolved projects must be in scope.
        let mut projects = distinct_projects(db, sql, uuid).await;
        if projects.is_empty() {
            return ScopeOutcome::Unresolved(
                "Target not found within your project access".to_string(),
            );
        }
        match entity {
            "sites" => {
                if let Some(sp) = fk("subproject_id") {
                    projects.extend(
                        distinct_projects(
                            db,
                            "SELECT project_id FROM subprojects WHERE id = $1",
                            sp,
                        )
                        .await,
                    );
                } else if let Some(p) = fk("project_id") {
                    projects.push(p);
                }
            }
            "subprojects" => {
                if let Some(p) = fk("project_id") {
                    projects.push(p);
                }
            }
            _ => {}
        }
        return ScopeOutcome::RequireAll(projects);
    }

    // Create: resolve the owning project from the request body's foreign key.
    if body.is_none() {
        return ScopeOutcome::Unresolved("Missing request body".to_string());
    }

    match entity {
        "sites" => match (fk("project_id"), fk("subproject_id")) {
            (Some(p), _) => ScopeOutcome::RequireAll(vec![p]),
            (None, Some(sp)) => require_all(
                distinct_projects(db, "SELECT project_id FROM subprojects WHERE id = $1", sp).await,
            ),
            (None, None) => {
                ScopeOutcome::Unresolved("Site create must specify project_id or subproject_id".to_string())
            }
        },
        "subprojects" => fk("project_id").map_or_else(
            || ScopeOutcome::Unresolved("Subproject create must specify project_id".to_string()),
            |p| ScopeOutcome::RequireAll(vec![p]),
        ),
        "site_parameters" | "notes" | "annotations" | "sensor_deployments" | "samples" => {
            match fk("site_id") {
                Some(site) => require_all(
                    distinct_projects(db, "SELECT project_id FROM sites WHERE id = $1", site).await,
                ),
                None => ScopeOutcome::Unresolved(format!("{entity} create must specify site_id")),
            }
        }
        "alarm_thresholds" => match fk("site_id") {
            Some(site) => require_all(
                distinct_projects(db, "SELECT project_id FROM sites WHERE id = $1", site).await,
            ),
            None => ScopeOutcome::Unresolved(
                "Project-scoped token cannot create a global (site-less) alarm threshold".to_string(),
            ),
        },
        "sensor_calibrations" => match fk("sensor_id") {
            Some(sensor) => require_any(
                distinct_projects(
                    db,
                    "SELECT DISTINCT s.project_id FROM sensor_deployments d \
                     JOIN sites s ON s.id = d.site_id WHERE d.sensor_id = $1",
                    sensor,
                )
                .await,
                "calibration",
            ),
            None => ScopeOutcome::Unresolved("Calibration create must specify sensor_id".to_string()),
        },
        "standard_curves" => match fk("sensor_id") {
            Some(sensor) => require_any_or_unbound(
                distinct_projects(
                    db,
                    "SELECT DISTINCT s.project_id FROM sensor_deployments d \
                     JOIN sites s ON s.id = d.site_id WHERE d.sensor_id = $1",
                    sensor,
                )
                .await,
                "standard curve",
            ),
            None => {
                ScopeOutcome::Unresolved("Standard curve create must specify sensor_id".to_string())
            }
        },
        "data_streams" => match fk("site_parameter_id") {
            Some(sp) => require_all(
                distinct_projects(
                    db,
                    "SELECT s.project_id FROM site_parameters sp JOIN sites s ON s.id = sp.site_id WHERE sp.id = $1",
                    sp,
                )
                .await,
            ),
            None => ScopeOutcome::Unresolved(
                "Project-scoped token cannot create an unpaired stream".to_string(),
            ),
        },
        other => ScopeOutcome::Global(format!("Project-scoped token cannot create '{other}'")),
    }
}

/// Build a `RequireAll` from resolved projects, or `Unresolved` when nothing resolved (a create FK
/// pointing at a missing/unbound site).
fn require_all(projects: Vec<Uuid>) -> ScopeOutcome {
    if projects.is_empty() {
        ScopeOutcome::Unresolved("Target is not bound to a project".to_string())
    } else {
        ScopeOutcome::RequireAll(projects)
    }
}

/// Build a `RequireAny` from resolved projects, or `Unresolved` when the entity resolves to no project
/// at all (e.g. a calibration for a never-deployed sensor), fail closed for a restricted caller.
fn require_any(projects: Vec<Uuid>, what: &str) -> ScopeOutcome {
    if projects.is_empty() {
        ScopeOutcome::Unresolved(format!("This {what} is not within your project access"))
    } else {
        ScopeOutcome::RequireAny(projects)
    }
}

/// As [`require_any`], but treating "resolves to no project" as no project dimension rather than as
/// a failed resolution.
///
/// Deployment-derived scope answers "which sites has this instrument stood at", and for a lab
/// instrument the answer is none: it is bench equipment shared across projects and is never
/// deployed. Failing closed there would make the entity uncreatable for exactly the population it
/// describes, rather than protecting a boundary, because an instrument bound to no project has no
/// boundary to cross. `sensors` itself is already global catalog for the same reason. A project
/// scoped token is still refused, and the caller's role still governs, so the level split between a
/// curve and the windowed calibration beside it is unaffected.
fn require_any_or_unbound(projects: Vec<Uuid>, what: &str) -> ScopeOutcome {
    if projects.is_empty() {
        ScopeOutcome::Global(format!(
            "Project-scoped token cannot create a {what} on an instrument bound to no project"
        ))
    } else {
        ScopeOutcome::RequireAny(projects)
    }
}

/// All distinct non-NULL `project_id`s a scope query resolves. A DB error yields an empty set, which
/// the callers treat as unresolved → fail closed.
async fn distinct_projects(db: &sea_orm::DatabaseConnection, sql: &str, id: Uuid) -> Vec<Uuid> {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    db.query_all(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        sql,
        [id.into()],
    ))
    .await
    .map(|rows| {
        rows.iter()
            .filter_map(|r| r.try_get::<Option<Uuid>>("", "project_id").ok().flatten())
            .collect()
    })
    .unwrap_or_default()
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
            .query_one(Statement::from_sql_and_values(
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

/// Row-filter confining a CRUD entity's *read* (list / get-by-id) to a restricted principal's
/// project set, or `None` for global-catalog, operational, and admin-only entities (reading shared
/// definitions like `parameters`/`constants` is intended). Built as a subquery so it adds no
/// round-trip and references only the entity's own columns; rows whose scoping column is NULL
/// (unpaired streams, site-less global thresholds) fall out by construction.
fn crud_read_scope_condition(entity: &str, projects: &[Uuid]) -> Option<sea_orm::Condition> {
    use crate::routes::private::{
        alarms::thresholds, annotations, data_streams, notes, projects as projects_entity,
        projects::subprojects, readings::samples, reprocessing_jobs, sensors,
        sensors::calibrations, sensors::deployments, sensors::standard_curves, sites,
        sites::parameters as site_parameters,
    };
    use sea_orm::{ColumnTrait, Condition};
    let ids = || projects.iter().copied();
    let expr = match entity {
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
        "sensors" => sensors::Column::Id.in_subquery(scoped_sensor_ids_query(projects)),
        "sensor_calibrations" => {
            calibrations::Column::SensorId.in_subquery(scoped_sensor_ids_query(projects))
        }
        "standard_curves" => {
            standard_curves::Column::SensorId.in_subquery(scoped_sensor_ids_query(projects))
        }
        // Mirrors `scope::project_of_job`: a job belongs to its site, else to the projects its
        // sensor is deployed into, and a job targeting neither is global. Returning early here
        // would hide global jobs (a CSV import targets no sensor) from the member who started one.
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

/// Read-side project-scope confinement for the CRUD entity routers. For a restricted principal (a
/// scoped API token, or a non-admin Keycloak member confined to their grant set) injects a CrudCrate
/// [`crudcrate::ScopeCondition`] so the generated handlers filter list results to that project set
/// and turn an out-of-scope get-by-id into a 404. No-op for unrestricted principals, for write
/// methods (mutations are confined by [`enforce_scope_on_crud`]), and for global/operational
/// entities. Custom sub-routes (e.g. `/sites/{id}/readings`) don't read the extension and keep their
/// own manual scope checks.
pub async fn inject_read_scope(request: Request, next: Next) -> Response {
    let scope = request
        .extensions()
        .get::<AuthContext>()
        .map_or(AccessScope::Unrestricted, AuthContext::access_scope);
    let Some(project_ids) = scope.project_ids() else {
        return next.run(request).await;
    };
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return next.run(request).await;
    }
    if let Some((entity, _id)) = parse_crud_target(request.uri().path())
        && let Some(condition) = crud_read_scope_condition(entity, &project_ids)
    {
        let mut request = request;
        request
            .extensions_mut()
            .insert(crudcrate::ScopeCondition::new(condition));
        return next.run(request).await;
    }
    next.run(request).await
}
