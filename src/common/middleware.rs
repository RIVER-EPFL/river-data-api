use axum::{
    extract::{FromRequestParts, Request},
    http::{Method, request::Parts},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::auth::{Capability, Role};
use crate::error::AppError;
use crate::routes::private::api_tokens::services::validate_bearer_token;

// Type alias for the Keycloak auth status used throughout this module.
type KcStatus = axum_keycloak_auth::KeycloakAuthStatus<
    Role,
    axum_keycloak_auth::decode::ProfileAndEmail,
>;

/// How the current request was authenticated.
#[derive(Debug, Clone)]
pub enum AuthContext {
    /// Authenticated via Keycloak JWT (admin UI, browser sessions).
    Keycloak {
        roles: Vec<Role>,
        /// Best-effort user identity (email, else preferred_username) for audit fields.
        email: Option<String>,
        /// Project confinement for a non-admin Keycloak user. `None` = global/cross-project
        /// access — the current behaviour for every Keycloak user. The field exists so the
        /// coming role-based RBAC (a user pidgeoned to one project) reuses the exact same
        /// scope-filtering plumbing as project-scoped API tokens; it stays `None` until then.
        scope: Option<Uuid>,
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

    /// Project scope this identity is confined to, if any. `None` = global/cross-project access.
    /// Sourced identically for every auth variant so scope-filtering is an identity-level concept,
    /// not a token-only one — future Keycloak per-project users flow through the same path.
    pub fn project_scope(&self) -> Option<Uuid> {
        match self {
            AuthContext::ApiToken { project_scope, .. } => *project_scope,
            AuthContext::Keycloak { scope, .. } => *scope,
        }
    }

    /// Whether this identity is granted a capability. The single source of truth for the
    /// Keycloak-role-vs-token-permission policy: every authenticated Keycloak user may read and
    /// write data, the Administrator role additionally writes metadata and holds `Admin`; an API
    /// token is limited to whichever of its four permission bits are set and never holds `Admin`
    /// (so a token can't reach token-minting / user-admin routes — defense in depth).
    pub fn allows(&self, cap: Capability) -> bool {
        match self {
            AuthContext::Keycloak { roles, .. } => {
                let is_admin = roles.contains(&Role::Administrator);
                match cap {
                    Capability::ReadMetadata | Capability::ReadData | Capability::WriteData => true,
                    Capability::WriteMetadata | Capability::Admin => is_admin,
                }
            }
            AuthContext::ApiToken { permissions, .. } => match cap {
                Capability::ReadMetadata => permissions.read_metadata,
                Capability::ReadData => permissions.read_data,
                Capability::WriteMetadata => permissions.write_metadata,
                Capability::WriteData => permissions.write_data,
                Capability::Admin => false,
            },
        }
    }
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
                let roles: Vec<Role> = token
                    .roles
                    .iter()
                    .map(|kr| kr.role().clone())
                    .collect();
                let email = {
                    let e = token.extra.email.email.trim();
                    if !e.is_empty() {
                        Some(e.to_string())
                    } else {
                        let u = token.extra.profile.preferred_username.trim();
                        (!u.is_empty()).then(|| u.to_string())
                    }
                };
                request
                    .extensions_mut()
                    .insert(AuthContext::Keycloak { roles, email, scope: None });
                return next.run(request).await;
            }
            axum_keycloak_auth::KeycloakAuthStatus::Failure(_) => {
                // Keycloak auth failed — fall through to try API token
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
        && let Some(token_model) = validate_bearer_token(&state.db, &header_value, &state.token_cache).await
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
            crate::routes::private::api_tokens::services::record_token_use(
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
        let token_hash = crate::routes::private::api_tokens::services::hash_token(raw);

        use crate::routes::private::sync::tokens_model as sync_service_tokens;
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

        if let Ok(Some(token)) = sync_service_tokens::Entity::find()
            .filter(sync_service_tokens::Column::TokenHash.eq(&token_hash))
            .one(&state.db)
            .await
            && token.expires_at.with_timezone(&chrono::Utc) >= chrono::Utc::now()
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
            return next.run(request).await;
        }
    }

    // No auth method succeeded
    AppError::Unauthorized("Valid Keycloak JWT or API token required".to_string()).into_response()
}

/// Shared capability gate: 401 if unauthenticated, 403 if the identity lacks `cap`, else proceed.
/// This is the single place the Keycloak-role-vs-token-permission policy is enforced (via
/// `AuthContext::allows`); the named `require_*` middlewares below are thin wrappers so route wiring
/// still reads as `require_read_data` etc. while the policy lives in exactly one function.
async fn require_capability(cap: Capability, request: Request, next: Next) -> Response {
    match request.extensions().get::<AuthContext>() {
        Some(ctx) if ctx.allows(cap) => next.run(request).await,
        Some(_) => AppError::Forbidden(format!("Requires {cap} capability")).into_response(),
        None => AppError::Unauthorized("Authentication required".to_string()).into_response(),
    }
}

/// Requires the `read_metadata` capability (granted to any authenticated principal with that bit).
pub async fn require_read_metadata(request: Request, next: Next) -> Response {
    require_capability(Capability::ReadMetadata, request, next).await
}

/// Requires the `read_data` capability.
pub async fn require_read_data(request: Request, next: Next) -> Response {
    require_capability(Capability::ReadData, request, next).await
}

/// Requires the `write_metadata` capability (Keycloak Administrator, or a token with write_metadata).
pub async fn require_write_metadata(request: Request, next: Next) -> Response {
    require_capability(Capability::WriteMetadata, request, next).await
}

/// Requires the `write_data` capability.
pub async fn require_write_data(request: Request, next: Next) -> Response {
    require_capability(Capability::WriteData, request, next).await
}

/// Method-aware `CrudCrate` gate: GET/HEAD need `read_metadata`, mutations need `write_metadata`.
pub async fn require_crud_permissions(request: Request, next: Next) -> Response {
    let cap = if matches!(*request.method(), Method::GET | Method::HEAD) {
        Capability::ReadMetadata
    } else {
        Capability::WriteMetadata
    };
    require_capability(cap, request, next).await
}

/// Requires the `Admin` capability — the Keycloak Administrator role only. NO API token can pass
/// (tokens never hold `Admin`): defense in depth for user management, token mutation, and sync
/// credential creation.
pub async fn require_admin(request: Request, next: Next) -> Response {
    require_capability(Capability::Admin, request, next).await
}

/// Extractor that yields the project scope from `AuthContext::ApiToken`, if any.
///
/// Returns `None` for Keycloak users or unscoped API tokens.
/// Handlers use this to filter queries by project when a token is scoped.
#[derive(Debug, Clone)]
pub struct ProjectScope(pub Option<Uuid>);

impl<S: Send + Sync> FromRequestParts<S> for ProjectScope {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let scope = parts
            .extensions
            .get::<AuthContext>()
            .and_then(AuthContext::project_scope);
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
        let scoped = parts
            .extensions
            .get::<AuthContext>()
            .and_then(AuthContext::project_scope)
            .is_some();
        if scoped {
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
        crate::routes::private::api_tokens::services::invalidate_token_cache(&state.token_cache)
            .await;
    }
    response
}

/// Deny project-scoped API tokens outright. Layered on operator/global action routes (sensor
/// adopt/swap, stream management, reprocess/backfill/aggregate refresh, merges, recalculate) that
/// either span projects or have no per-project target — work a per-client logger key has no reason
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

/// Mutating-CRUD project-scope guard for API tokens, layered on the entity router. For a
/// project-scoped token performing a create/update/delete, resolves the target row's owning
/// project and rejects anything outside the token's scope. **Fails closed**: any entity whose
/// owning project can't be resolved — including the global catalog (`parameters`, `sensors`,
/// `constants`, `standard_curves`, …) — is denied, so a per-client key can never mutate shared or
/// cross-project metadata. Keycloak users and unscoped API tokens pass through untouched (and pay
/// no body-buffering cost, since they return before the body is read).
pub async fn enforce_token_scope_on_crud(
    state: axum::extract::State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let scope = match request.extensions().get::<AuthContext>() {
        Some(AuthContext::ApiToken {
            project_scope: Some(p),
            ..
        }) => *p,
        _ => return next.run(request).await,
    };

    if matches!(*request.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(request).await;
    }

    let path = request.uri().path().to_string();
    let Some((entity, id)) = parse_crud_target(&path) else {
        return AppError::Forbidden(
            "Project-scoped token cannot perform this operation".to_string(),
        )
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

    match resolve_scope_project(&state.db, &entity, id.as_deref(), json.as_ref()).await {
        ScopeOutcome::Project(project) if project == scope => {}
        ScopeOutcome::Project(_) => {
            return AppError::Forbidden("Token is scoped to a different project".to_string())
                .into_response();
        }
        ScopeOutcome::Deny(msg) => return AppError::Forbidden(msg).into_response(),
    }

    let request = Request::from_parts(parts, axum::body::Body::from(bytes));
    next.run(request).await
}

enum ScopeOutcome {
    Project(Uuid),
    Deny(String),
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
    // Update / delete: resolve the owning project from the existing row.
    if let Some(id) = id {
        let Ok(uuid) = Uuid::parse_str(id) else {
            return ScopeOutcome::Deny("Project-scoped token cannot resolve target".to_string());
        };
        let sql = match entity {
            "sites" => "SELECT project_id FROM sites WHERE id = $1",
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
            "data_streams" => {
                "SELECT s.project_id FROM data_streams ds JOIN site_parameters sp ON sp.id = ds.site_parameter_id JOIN sites s ON s.id = sp.site_id WHERE ds.id = $1"
            }
            other => {
                return ScopeOutcome::Deny(format!(
                    "Project-scoped token cannot modify '{other}'"
                ));
            }
        };
        return project_from_query(db, sql, uuid).await;
    }

    // Create: resolve the owning project from the request body's foreign key.
    let Some(body) = body else {
        return ScopeOutcome::Deny("Missing request body".to_string());
    };
    let fk = |key: &str| {
        body.get(key)
            .and_then(serde_json::Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
    };

    match entity {
        "sites" => fk("project_id").map_or_else(
            || ScopeOutcome::Deny("Site create must specify project_id".to_string()),
            ScopeOutcome::Project,
        ),
        "site_parameters" | "notes" | "annotations" | "sensor_deployments" => match fk("site_id") {
            Some(site) => {
                project_from_query(db, "SELECT project_id FROM sites WHERE id = $1", site).await
            }
            None => ScopeOutcome::Deny(format!("{entity} create must specify site_id")),
        },
        "alarm_thresholds" => match fk("site_id") {
            Some(site) => {
                project_from_query(db, "SELECT project_id FROM sites WHERE id = $1", site).await
            }
            None => ScopeOutcome::Deny(
                "Project-scoped token cannot create a global (site-less) alarm threshold"
                    .to_string(),
            ),
        },
        "data_streams" => match fk("site_parameter_id") {
            Some(sp) => {
                project_from_query(
                    db,
                    "SELECT s.project_id FROM site_parameters sp JOIN sites s ON s.id = sp.site_id WHERE sp.id = $1",
                    sp,
                )
                .await
            }
            None => ScopeOutcome::Deny(
                "Project-scoped token cannot create an unpaired stream".to_string(),
            ),
        },
        other => ScopeOutcome::Deny(format!("Project-scoped token cannot create '{other}'")),
    }
}

async fn project_from_query(
    db: &sea_orm::DatabaseConnection,
    sql: &str,
    id: Uuid,
) -> ScopeOutcome {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    match db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            [id.into()],
        ))
        .await
    {
        Ok(Some(row)) => match row.try_get::<Option<Uuid>>("", "project_id").ok().flatten() {
            Some(project) => ScopeOutcome::Project(project),
            None => ScopeOutcome::Deny("Target is not bound to a project".to_string()),
        },
        Ok(None) => ScopeOutcome::Deny("Target not found within token scope".to_string()),
        Err(_) => ScopeOutcome::Deny("Could not resolve target project".to_string()),
    }
}

/// Reject the request if a project-scoped API token is writing to any site outside its project.
/// No-op for unscoped callers (Keycloak users and unscoped API tokens both pass `scope = None`).
/// `site_ids` are the distinct sites the request would touch; an unknown site is also rejected.
pub async fn enforce_project_scope_for_sites(
    db: &sea_orm::DatabaseConnection,
    scope: Option<Uuid>,
    site_ids: &[Uuid],
) -> Result<(), AppError> {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    let Some(scope_project) = scope else {
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
                .is_some_and(|pid| pid == scope_project),
            None => false,
        };
        if !in_scope {
            return Err(AppError::Forbidden(
                "Token is scoped to a different project".to_string(),
            ));
        }
    }
    Ok(())
}

/// Read-side scope filter: the site ids belonging to a scoped principal's project, or `None` when
/// the principal is unscoped (global access — no filtering). This is the read mirror of
/// `enforce_project_scope_for_sites`: handlers that return rows keyed by `site_id` pass the returned
/// list as `site_id = ANY($n)` so a project-scoped key sees only its project's data and inventory.
/// An empty `Some(vec![])` (a scope whose project has no sites) correctly filters everything out.
pub async fn scope_site_ids(
    db: &sea_orm::DatabaseConnection,
    scope: Option<Uuid>,
) -> Result<Option<Vec<Uuid>>, AppError> {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    let Some(project) = scope else {
        return Ok(None);
    };
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT id FROM sites WHERE project_id = $1",
            [project.into()],
        ))
        .await
        .map_err(AppError::Database)?;
    let ids = rows
        .iter()
        .filter_map(|r| r.try_get::<Uuid>("", "id").ok())
        .collect();
    Ok(Some(ids))
}

/// Whether a sensor is visible to a scoped principal: `true` if unscoped, otherwise `true` only when
/// the sensor has at least one deployment to a site within the scoped project. Single-resource
/// sensor read endpoints use this to 404 a cross-project sensor (rather than confirm its existence)
/// before filtering its per-site rows to the project.
pub async fn sensor_in_scope(
    db: &sea_orm::DatabaseConnection,
    scope: Option<Uuid>,
    sensor_id: Uuid,
) -> Result<bool, AppError> {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    let Some(project) = scope else {
        return Ok(true);
    };
    let row = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT EXISTS (\
               SELECT 1 FROM sensor_deployments d JOIN sites s ON s.id = d.site_id \
               WHERE d.sensor_id = $1 AND s.project_id = $2\
             ) AS in_scope",
            [sensor_id.into(), project.into()],
        ))
        .await
        .map_err(AppError::Database)?;
    Ok(row
        .and_then(|r| r.try_get::<bool>("", "in_scope").ok())
        .unwrap_or(false))
}
