//! Who may reach the API and as what: the bearer tokens minted here, and the realm's users,
//! roles and per-project grants proxied from Keycloak.
//!
//! The realm is the directory; river-data stores no user row, only the grants keyed by a
//! Keycloak `sub`.

use axum::Json;
use axum::Router;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use chrono::DateTime;
use chrono::Utc;
use sea_orm::ActiveModelTrait;
use sea_orm::ConnectionTrait;
use sea_orm::EntityTrait;
use sea_orm::FromQueryResult;
use sea_orm::IntoActiveModel;
use sea_orm::QueryOrder;
use sea_orm::QuerySelect;
use sea_orm::Set;
use sea_orm::Statement;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use super::models as model;
use super::models::ApiToken;
use super::models::*;
use super::service::invalidate_token_cache;
use super::service::mint_api_token;
use super::service::*;
use crate::common::AppState;
use crate::common::authz::RIVER_ROLE_NAMES;
use crate::common::authz::Role;
use crate::common::paging::Window;
use crate::common::paging::content_range;
use crate::error::AppError;
use crate::error::AppResult;

/// Revoke an API token (soft-disable). The token stops working on the next request because the
/// validation cache is invalidated here. Admin-only (mounted behind `require_admin`).
#[utoipa::path(
    post,
    path = "/api/tokens/{id}/revoke",
    params(("id" = Uuid, Path, description = "Token id")),
    responses(
        (status = 200, description = "Token revoked", body = ApiToken),
        (status = 404, description = "Token not found"),
    ),
    tag = "tokens"
)]
pub async fn revoke_token(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<ApiToken>> {
    let existing = model::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Token not found".to_string()))?;

    let mut active = existing.into_active_model();
    active.is_active = Set(false);
    let updated = active.update(&state.db).await?;

    invalidate_token_cache(&state.token_cache).await;
    Ok(Json(ApiToken::from(updated)))
}

/// Rotate an API token: mint a new secret while preserving all metadata (name, description,
/// project scope, permissions, rate limit, expiry). The previous secret stops working immediately
/// (cache invalidated); the new secret is returned once in `token`. Admin-only.
#[utoipa::path(
    post,
    path = "/api/tokens/{id}/rotate",
    params(("id" = Uuid, Path, description = "Token id")),
    responses(
        (status = 200, description = "Token rotated; new secret in `token`", body = ApiToken),
        (status = 404, description = "Token not found"),
    ),
    tag = "tokens"
)]
pub async fn rotate_token(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<ApiToken>> {
    let existing = model::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Token not found".to_string()))?;

    let minted = mint_api_token();
    let mut active = existing.into_active_model();
    // Rotation is a purely cryptographic operation: replace the secret, but preserve admin state
    // (a revoked token stays revoked, rotating it must not silently re-enable it).
    active.token_hash = Set(minted.token_hash);
    active.token_prefix = Set(minted.token_prefix);
    let updated = active.update(&state.db).await?;

    invalidate_token_cache(&state.token_cache).await;
    let mut out = ApiToken::from(updated);
    out.token = Some(minted.raw_token);
    Ok(Json(out))
}

/// One recorded use of an API token from the forensic audit log.
#[derive(Debug, Serialize, ToSchema, sea_orm::FromQueryResult)]
pub struct TokenUsageEntry {
    pub method: String,
    pub path: String,
    pub status_code: i32,
    #[schema(required)]
    pub project_scope: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Recent usage of an API token (most recent first, capped at 200) from the forensic audit log.
/// Admin-only, like all token management. Empty when auditing is disabled or the token is unused.
#[utoipa::path(
    get,
    path = "/api/tokens/{id}/usage",
    params(("id" = Uuid, Path, description = "Token id")),
    responses(
        (status = 200, description = "Recent token usage", body = [TokenUsageEntry]),
    ),
    tag = "tokens"
)]
pub async fn token_usage(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<TokenUsageEntry>>> {
    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT method, path, status_code, project_scope, created_at \
             FROM api_token_audit_log WHERE token_id = $1 \
             ORDER BY created_at DESC LIMIT 200",
            [id.into()],
        ))
        .await?;

    let entries = rows
        .iter()
        .map(|r| Ok(TokenUsageEntry::from_query_result(r, "")?))
        .collect::<AppResult<Vec<_>>>()?;

    Ok(Json(entries))
}

/// Distinct HTTP status codes present in the audit log, used to populate the admin filter dropdown
/// with the values that actually occur rather than a free-form number box.
#[derive(Debug, Serialize, ToSchema)]
pub struct AuditStatusCodes {
    pub status_codes: Vec<i32>,
}

/// Distinct `status_code` values recorded in `api_token_audit_log`, ascending. Admin-only, read-only.
#[utoipa::path(
    get,
    path = "/api/api_token_audit_logs/distinct/status_codes",
    responses(
        (status = 200, description = "Distinct status codes recorded in the audit log", body = AuditStatusCodes),
    ),
    tag = "tokens"
)]
pub async fn distinct_status_codes(
    State(state): State<AppState>,
) -> AppResult<Json<AuditStatusCodes>> {
    let status_codes = model::audit_log::Entity::find()
        .select_only()
        .column(model::audit_log::Column::StatusCode)
        .distinct()
        .order_by_asc(model::audit_log::Column::StatusCode)
        .into_tuple::<i32>()
        .all(&state.db)
        .await?;

    Ok(Json(AuditStatusCodes { status_codes }))
}

// --- The realm's users and roles ---

/// The page a caller naming no range is served, and the largest one it may ask for.
const DEFAULT_PAGE_SIZE: u64 = 25;
const MAX_PAGE_SIZE: u64 = 1000;

/// List Keycloak users holding any riverdata access role, with optional filtering by
/// search query (username, email, firstName, lastName) and admin flag. Proxies to Keycloak's
/// admin API. Requires Keycloak Administrator role (`require_admin`).
#[utoipa::path(
    get,
    path = "/api/users",
    params(ListQuery),
    responses(
        (status = 200, description = "User list with Content-Range header", body = Vec<KeycloakUser>),
        (status = 503, description = "Keycloak admin client not configured"),
    ),
    tag = "admin"
)]
pub async fn list_users(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<impl IntoResponse, AppError> {
    let token = get_admin_token(&state).await?;
    let client = admin_client(&state)?;
    let base = admin_base_url(&state)?;

    let window = Window::from_range(query.range.as_deref(), DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE);
    let first = usize::try_from(window.offset).unwrap_or(usize::MAX);
    let max = usize::try_from(window.limit).unwrap_or(usize::MAX);

    // Parse filters
    let filter_json = query
        .filter
        .as_ref()
        .and_then(|f| serde_json::from_str::<serde_json::Value>(f).ok());
    let search = filter_json.as_ref().and_then(|v| {
        v.get("q")
            .or_else(|| v.get("search"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_lowercase())
    });
    let admin_filter = filter_json
        .as_ref()
        .and_then(|v| v.get("admin").and_then(|a| a.as_bool()));

    // Fetch only users holding a riverdata role (not the entire realm; it is LDAP-federated
    // and contains every EPFL account). Union members of every level so admin-only users appear
    // too. A missing role (a level not yet created in Keycloak) yields no members via
    // `fetch_role_users`; any real failure (forbidden, server error) still propagates.
    let admin_role = Role::Administrator.to_string();
    let role_member_lists = effective_role_members(client, &token, &base).await?;
    // Attribute each level to the user that holds it, first-seen order preserved. The roles a user
    // collects across the membership lists ARE their access levels, no per-user role fetch needed.
    let mut order: Vec<String> = Vec::new();
    let mut by_id: std::collections::HashMap<String, (serde_json::Value, Vec<String>)> =
        std::collections::HashMap::new();
    for (role, members) in role_member_lists {
        for u in members {
            let Some(id) = u["id"].as_str().map(str::to_string) else {
                continue;
            };
            if let Some((_, roles)) = by_id.get_mut(&id) {
                if !roles.contains(&role) {
                    roles.push(role.clone());
                }
            } else {
                order.push(id.clone());
                by_id.insert(id, (u, vec![role.clone()]));
            }
        }
    }
    let mut users: Vec<KeycloakUser> = order
        .into_iter()
        .map(|id| {
            let (u, mut roles) = by_id.remove(&id).expect("id came from order");
            roles.sort_by_key(|r| RIVER_ROLE_NAMES.iter().position(|n| n == r));
            simplify_user(&u, roles)
        })
        .collect();

    // Apply admin filter
    if let Some(want_admin) = admin_filter {
        users.retain(|u| u.roles.iter().any(|r| r == &admin_role) == want_admin);
    }

    // Apply search filter (case-insensitive on username, email, firstName, lastName)
    if let Some(ref q) = search {
        users.retain(|u| {
            [&u.username, &u.email, &u.first_name, &u.last_name]
                .iter()
                .any(|field| {
                    field
                        .as_deref()
                        .is_some_and(|v| v.to_lowercase().contains(q))
                })
        });
    }

    let total = users.len();
    let page: Vec<KeycloakUser> = users.into_iter().skip(first).take(max).collect();
    let headers = content_range(window.offset, page.len(), total as u64, "users");

    Ok((headers, Json(page)))
}

/// Search the realm's user directory (LDAP-federated in production, so this reaches every
/// EPFL account). Each result carries its current realm roles so callers can tell who already
/// has river-data access. Used by the UI's add-user flow. Requires `require_admin`.
#[utoipa::path(
    get,
    path = "/api/users/search",
    params(SearchQuery),
    responses(
        (status = 200, description = "Matching users with their realm roles", body = Vec<KeycloakUser>),
        (status = 503, description = "Keycloak admin client not configured"),
    ),
    tag = "admin"
)]
pub async fn search_users(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> AppResult<Json<Vec<KeycloakUser>>> {
    let token = get_admin_token(&state).await?;
    let client = admin_client(&state)?;
    let base = admin_base_url(&state)?;

    let resp = client
        .http_client
        .get(format!("{base}/users"))
        .bearer_auth(&token)
        .query(&[
            ("search", query.q.as_str()),
            ("max", "20"),
            ("briefRepresentation", "true"),
        ])
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Keycloak request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "Keycloak user search failed ({status}): {body}"
        )));
    }

    let kc_users: Vec<serde_json::Value> = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to parse user search: {e}")))?;

    let role_futures: Vec<_> = kc_users
        .iter()
        .map(|u| {
            let user_id = u["id"].as_str().unwrap_or_default().to_string();
            let token = token.clone();
            let base = base.clone();
            async move { fetch_user_roles(client, &token, &base, &user_id).await }
        })
        .collect();
    let all_roles = futures::future::join_all(role_futures).await;

    let mut users: Vec<KeycloakUser> = Vec::with_capacity(kc_users.len());
    for (u, roles) in kc_users.iter().zip(all_roles) {
        users.push(simplify_user(u, roles?));
    }

    Ok(Json(users))
}

/// Get a Keycloak user by ID with their realm roles attached. Requires `require_admin`.
#[utoipa::path(
    get,
    path = "/api/users/{id}",
    params(("id" = String, Path, description = "Keycloak user UUID")),
    responses(
        (status = 200, description = "User detail", body = KeycloakUser),
        (status = 404, description = "User not found"),
    ),
    tag = "admin"
)]
pub async fn get_user(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<KeycloakUser>> {
    let token = get_admin_token(&state).await?;
    let client = admin_client(&state)?;
    let base = admin_base_url(&state)?;

    let resp = client
        .http_client
        .get(format!("{base}/users/{id}"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Keycloak request failed: {e}")))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(AppError::NotFound("User not found".to_string()));
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "Keycloak request failed ({status}): {body}"
        )));
    }

    let user: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to parse user: {e}")))?;

    // Fetch realm role mappings
    let roles = fetch_user_roles(client, &token, &base, &id).await?;

    Ok(Json(simplify_user(&user, roles)))
}

/// Update a Keycloak user (partial JSON merge). Requires `require_admin`.
#[utoipa::path(
    put,
    path = "/api/users/{id}",
    params(("id" = String, Path, description = "Keycloak user UUID")),
    request_body(content = Object, description = "Partial user fields to update"),
    responses(
        (status = 200, description = "User updated", body = KeycloakUser),
        (status = 404, description = "User not found"),
    ),
    tag = "admin"
)]
pub async fn update_user(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<serde_json::Value>,
) -> AppResult<Json<KeycloakUser>> {
    let token = get_admin_token(&state).await?;
    let client = admin_client(&state)?;
    let base = admin_base_url(&state)?;

    // Get current user to merge with updates
    let current_resp = client
        .http_client
        .get(format!("{base}/users/{id}"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Keycloak request failed: {e}")))?;

    if !current_resp.status().is_success() {
        return Err(AppError::NotFound("User not found".to_string()));
    }

    let mut current: serde_json::Value = current_resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to parse user: {e}")))?;

    // Merge updatable fields
    for key in ["email", "firstName", "lastName", "enabled"] {
        if let Some(v) = req.get(key) {
            current[key] = v.clone();
        }
    }

    let resp = client
        .http_client
        .put(format!("{base}/users/{id}"))
        .bearer_auth(&token)
        .json(&current)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Keycloak update failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "Keycloak update failed ({status}): {body}"
        )));
    }

    // Handle role assignment if roles are included in the update
    let roles = if let Some(roles) = req.get("roles").and_then(|r| r.as_array()) {
        let role_names: Vec<String> = roles
            .iter()
            .filter_map(|r| r.as_str().map(String::from))
            .collect();
        set_user_roles(client, &token, &base, &id, &role_names).await?;
        role_names
    } else {
        fetch_user_roles(client, &token, &base, &id).await?
    };

    invalidate_cached_access(&state, &id).await;

    Ok(Json(simplify_user(&current, roles)))
}

/// Delete a Keycloak user. Requires `require_admin`.
#[utoipa::path(
    delete,
    path = "/api/users/{id}",
    params(("id" = String, Path, description = "Keycloak user UUID")),
    responses(
        (status = 200, description = "User deleted", body = DeletedUser),
        (status = 404, description = "User not found"),
    ),
    tag = "admin"
)]
pub async fn delete_user(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<DeletedUser>> {
    let token = get_admin_token(&state).await?;
    let client = admin_client(&state)?;
    let base = admin_base_url(&state)?;

    let resp = client
        .http_client
        .delete(format!("{base}/users/{id}"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Keycloak request failed: {e}")))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(AppError::NotFound("User not found".to_string()));
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "Keycloak delete failed ({status}): {body}"
        )));
    }

    invalidate_cached_access(&state, &id).await;

    Ok(Json(DeletedUser { id }))
}

/// Set the realm roles for a user (overwrites; not additive). Requires `require_admin`.
#[utoipa::path(
    post,
    path = "/api/users/{id}/roles",
    params(("id" = String, Path, description = "Keycloak user UUID")),
    request_body = AssignRolesRequest,
    responses(
        (status = 200, description = "Roles updated", body = RolesAssigned),
        (status = 404, description = "User not found"),
    ),
    tag = "admin"
)]
pub async fn assign_roles(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<AssignRolesRequest>,
) -> AppResult<Json<RolesAssigned>> {
    let token = get_admin_token(&state).await?;
    let client = admin_client(&state)?;
    let base = admin_base_url(&state)?;

    set_user_roles(client, &token, &base, &id, &req.roles).await?;

    invalidate_cached_access(&state, &id).await;

    Ok(Json(RolesAssigned { success: true }))
}

/// List the Keycloak riverdata access roles (`riverdata-admin` / `-manager` / `-river` / `-intern`).
/// Used by the UI's role-assignment picker. Requires `require_admin`.
#[utoipa::path(
    get,
    path = "/api/roles",
    responses(
        (status = 200, description = "Realm roles", body = [KeycloakRole]),
    ),
    tag = "admin"
)]
pub async fn list_roles(State(state): State<AppState>) -> AppResult<Json<Vec<serde_json::Value>>> {
    let token = get_admin_token(&state).await?;
    let client = admin_client(&state)?;
    let base = admin_base_url(&state)?;

    let resp = client
        .http_client
        .get(format!("{base}/roles"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Keycloak request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "Failed to fetch roles ({status}): {body}"
        )));
    }
    let roles: Vec<KeycloakRole> = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to parse roles: {e}")))?;

    // Only the riverdata access levels are exposed, so the role picker cannot assign Keycloak
    // internals or the bare `admin` role.
    let roles: Vec<serde_json::Value> = roles
        .into_iter()
        .filter(|r| RIVER_ROLE_NAMES.contains(&r.name.as_str()))
        .map(|r| serde_json::json!({ "id": r.id, "name": r.name }))
        .collect();

    Ok(Json(roles))
}

#[utoipa::path(
    get,
    path = "/api/users/{id}/grants",
    params(("id" = String, Path, description = "Keycloak user id (sub)")),
    responses((status = 200, description = "The projects the user is granted", body = Vec<crate::routes::private::me::GrantedProject>)),
    tag = "users"
)]
pub async fn list_user_grants(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Vec<crate::routes::private::me::GrantedProject>>> {
    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT p.id, p.name FROM user_project_grants g \
             JOIN projects p ON p.id = g.project_id \
             WHERE g.user_sub = $1 ORDER BY p.name",
            [id.into()],
        ))
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    // A row that will not decode is an error: silently dropping it would answer with a shorter
    // grant list than the user holds.
    let grants = rows
        .iter()
        .map(|r| GrantRow::from_query_result(r, ""))
        .collect::<Result<Vec<GrantRow>, _>>()?
        .into_iter()
        .map(|g| crate::routes::private::me::GrantedProject {
            project_id: g.id,
            name: g.name,
        })
        .collect();
    Ok(Json(grants))
}

/// effect within one request. Requires `require_admin`.
#[utoipa::path(
    put,
    path = "/api/users/{id}/grants",
    params(("id" = String, Path, description = "Keycloak user id (sub)")),
    request_body = SetGrantsRequest,
    responses((status = 200, description = "The grant set after the write", body = SetGrantsResponse)),
    tag = "users"
)]
pub async fn set_user_grants(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Path(id): Path<String>,
    Json(req): Json<SetGrantsRequest>,
) -> AppResult<Json<SetGrantsResponse>> {
    use sea_orm::TransactionTrait;
    let granted_by = auth.keycloak_sub().unwrap_or("").to_string();
    let txn = state
        .db
        .begin()
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    txn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "DELETE FROM user_project_grants WHERE user_sub = $1",
        [id.clone().into()],
    ))
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;
    for project_id in &req.project_ids {
        txn.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO user_project_grants (user_sub, project_id, granted_by) VALUES ($1, $2, $3) \
             ON CONFLICT (user_sub, project_id) DO NOTHING",
            [id.clone().into(), (*project_id).into(), granted_by.clone().into()],
        ))
        .await
        .map_err(|e| AppError::BadRequest(format!("grant insert failed (unknown project?): {e}")))?;
    }
    txn.commit()
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    state.grants_cache.invalidate(&id).await;

    Ok(Json(SetGrantsResponse {
        success: true,
        count: req.project_ids.len(),
    }))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_users))
        .route("/search", get(search_users))
        .route("/{id}", get(get_user).put(update_user).delete(delete_user))
        .route("/{id}/roles", axum::routing::post(assign_roles))
        .route("/{id}/grants", get(list_user_grants).put(set_user_grants))
}
