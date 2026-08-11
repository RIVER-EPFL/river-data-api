use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue},
    response::IntoResponse,
    routing::get,
};
use chrono::Utc;
use sea_orm::{ConnectionTrait, Statement};
use serde::Deserialize;
use utoipa::ToSchema;

use crate::common::AppState;
use crate::common::authz::{RIVER_ROLE_NAMES, Role};
use crate::common::state::KeycloakAdmin;
use crate::error::{AppError, AppResult};

fn has_riverdata_role(roles: &[String]) -> bool {
    roles.iter().any(|r| RIVER_ROLE_NAMES.contains(&r.as_str()))
}

/// Anti-backdoor hook: a user's bot access must not outlive their system access. On any change to a
/// user's roles / enabled flag / existence, drop their cached role so the bot re-resolves on the next
/// command, and — when they no longer have access — deactivate their linked Telegram chats outright
/// rather than waiting for the reconciliation sweep. Best-effort: never fails the user-management op.
async fn revoke_telegram_access(state: &AppState, sub: &str, still_has_access: bool) {
    state.authorizer.invalidate(sub).await;
    // A user's project visibility must re-resolve the moment their access changes (role edit,
    // disable, delete). Cheap, and it means a disabled user's cached grants can't linger.
    state.grants_cache.invalidate(sub).await;
    if !still_has_access {
        let res = state
            .db
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE telegram_identities SET is_active = FALSE, updated_at = NOW() \
                 WHERE linked_keycloak_sub = $1",
                [sub.into()],
            ))
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "failed to deactivate telegram identities on access change");
        }
    }
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct ListQuery {
    /// React-admin style range, e.g. "[0,9]"
    pub range: Option<String>,
    /// React-admin style filter, e.g. {"q":"john"}
    pub filter: Option<String>,
}

#[derive(Debug, Deserialize, serde::Serialize, ToSchema)]
pub struct KeycloakRole {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AssignRolesRequest {
    pub roles: Vec<String>,
}

pub(crate) async fn get_admin_token(state: &AppState) -> AppResult<String> {
    let admin = state
        .keycloak_admin
        .as_ref()
        .ok_or_else(|| AppError::Internal("Keycloak admin not configured".to_string()))?;

    // Check cache (reuse if >30s before expiry)
    {
        let cache = admin.token_cache.lock().await;
        if let Some((token, expiry)) = cache.as_ref()
            && *expiry > Utc::now() + chrono::Duration::seconds(30) {
                return Ok(token.clone());
            }
    }

    let url = format!(
        "{}/realms/{}/protocol/openid-connect/token",
        state.config.keycloak_url.as_ref()
            .ok_or_else(|| AppError::ServiceUnavailable("Keycloak not configured".to_string()))?,
        state.config.keycloak_realm.as_ref()
            .ok_or_else(|| AppError::ServiceUnavailable("Keycloak not configured".to_string()))?,
    );

    let resp = admin
        .http_client
        .post(&url)
        .form(&[
            ("grant_type", "client_credentials"),
            ("client_id", &admin.client_id),
            ("client_secret", &admin.client_secret),
        ])
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Keycloak token request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "Keycloak token request failed ({status}): {body}"
        )));
    }

    #[derive(Deserialize)]
    struct TokenResponse {
        access_token: String,
        expires_in: i64,
    }

    let token_resp: TokenResponse = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to parse token response: {e}")))?;

    let expiry = Utc::now() + chrono::Duration::seconds(token_resp.expires_in);
    let token = token_resp.access_token.clone();

    {
        let mut cache = admin.token_cache.lock().await;
        *cache = Some((token_resp.access_token, expiry));
    }

    Ok(token)
}

pub(crate) fn admin_base_url(state: &AppState) -> AppResult<String> {
    Ok(format!(
        "{}/admin/realms/{}",
        state.config.keycloak_url.as_ref()
            .ok_or_else(|| AppError::ServiceUnavailable("Keycloak not configured".to_string()))?,
        state.config.keycloak_realm.as_ref()
            .ok_or_else(|| AppError::ServiceUnavailable("Keycloak not configured".to_string()))?,
    ))
}

pub(crate) fn admin_client(state: &AppState) -> AppResult<&KeycloakAdmin> {
    state.keycloak_admin.as_ref()
        .ok_or_else(|| AppError::ServiceUnavailable("Keycloak not configured".to_string()))
}

/// Transform a Keycloak user JSON into our simplified format.
fn simplify_user(u: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": u["id"],
        "username": u["username"],
        "email": u["email"],
        "firstName": u["firstName"],
        "lastName": u["lastName"],
        "enabled": u["enabled"],
        "createdTimestamp": u["createdTimestamp"],
    })
}

/// List Keycloak users holding any riverdata access role, with optional filtering by
/// search query (username, email, firstName, lastName) and admin flag. Proxies to Keycloak's
/// admin API. Requires Keycloak Administrator role (`require_admin`).
#[utoipa::path(
    get,
    path = "/users",
    params(ListQuery),
    responses(
        (status = 200, description = "User list with Content-Range header", body = Object),
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

    // Parse React Admin range: [start, end] (inclusive)
    let (first, max) = if let Some(range) = &query.range {
        let r: Vec<usize> = serde_json::from_str(range).unwrap_or_else(|_| vec![0, 24]);
        let start = r.first().copied().unwrap_or(0);
        let end = r.get(1).copied().unwrap_or(24);
        (start, end.saturating_sub(start).saturating_add(1))
    } else {
        (0, 25)
    };

    // Parse filters
    let filter_json = query.filter.as_ref().and_then(|f| {
        serde_json::from_str::<serde_json::Value>(f).ok()
    });
    let search = filter_json.as_ref().and_then(|v| {
        v.get("q")
            .or_else(|| v.get("search"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_lowercase())
    });
    let admin_filter = filter_json.as_ref().and_then(|v| {
        v.get("admin").and_then(|a| a.as_bool())
    });

    // Fetch only users holding a riverdata role (not the entire realm — it is LDAP-federated
    // and contains every EPFL account). Union members of every level so admin-only users appear
    // too. A missing role (a level not yet created in Keycloak) yields no members via
    // `fetch_role_users`; any real failure (forbidden, server error) still propagates.
    let admin_role = Role::Administrator.to_string();
    let role_member_lists = futures::future::join_all(
        RIVER_ROLE_NAMES
            .iter()
            .map(|role| fetch_role_users(client, &token, &base, role)),
    )
    .await;
    // Attribute each level to the user that holds it, first-seen order preserved. The roles a user
    // collects across the membership lists ARE their access levels — no per-user role fetch needed.
    let mut order: Vec<String> = Vec::new();
    let mut by_id: std::collections::HashMap<String, (serde_json::Value, Vec<String>)> =
        std::collections::HashMap::new();
    for (role, members) in RIVER_ROLE_NAMES.iter().zip(role_member_lists) {
        for u in members? {
            let Some(id) = u["id"].as_str().map(str::to_string) else { continue };
            if let Some((_, roles)) = by_id.get_mut(&id) {
                roles.push((*role).to_string());
            } else {
                order.push(id.clone());
                by_id.insert(id, (u, vec![(*role).to_string()]));
            }
        }
    }
    let mut users: Vec<serde_json::Value> = order
        .into_iter()
        .map(|id| {
            let (u, roles) = by_id.remove(&id).expect("id came from order");
            let mut user = simplify_user(&u);
            user["roles"] = serde_json::json!(roles);
            user
        })
        .collect();

    // Apply admin filter
    if let Some(want_admin) = admin_filter {
        users.retain(|u| {
            let is_admin = u["roles"]
                .as_array()
                .is_some_and(|r| r.iter().any(|v| v.as_str() == Some(&admin_role)));
            is_admin == want_admin
        });
    }

    // Apply search filter (case-insensitive on username, email, firstName, lastName)
    if let Some(ref q) = search {
        users.retain(|u| {
            ["username", "email", "firstName", "lastName"]
                .iter()
                .any(|field| {
                    u[*field]
                        .as_str()
                        .is_some_and(|v| v.to_lowercase().contains(q))
                })
        });
    }

    let total = users.len();
    let page: Vec<serde_json::Value> = users.into_iter().skip(first).take(max).collect();
    let end = first + page.len().saturating_sub(1);
    let content_range = format!("users {first}-{end}/{total}");

    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Range",
        HeaderValue::from_str(&content_range).unwrap(),
    );

    Ok((headers, Json(page)))
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct SearchQuery {
    /// Search string matched by Keycloak against username, email, first and last name.
    pub q: String,
}

/// Search the realm's user directory (LDAP-federated in production, so this reaches every
/// EPFL account). Each result carries its current realm roles so callers can tell who already
/// has river-data access. Used by the UI's add-user flow. Requires `require_admin`.
#[utoipa::path(
    get,
    path = "/users/search",
    params(SearchQuery),
    responses(
        (status = 200, description = "Matching users with their realm roles", body = Object),
        (status = 503, description = "Keycloak admin client not configured"),
    ),
    tag = "admin"
)]
pub async fn search_users(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> AppResult<Json<Vec<serde_json::Value>>> {
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

    let mut users: Vec<serde_json::Value> = Vec::with_capacity(kc_users.len());
    for (u, roles) in kc_users.iter().zip(all_roles) {
        let mut user = simplify_user(u);
        user["roles"] = serde_json::json!(roles?);
        users.push(user);
    }

    Ok(Json(users))
}

/// Get a Keycloak user by ID with their realm roles attached. Requires `require_admin`.
#[utoipa::path(
    get,
    path = "/users/{id}",
    params(("id" = String, Path, description = "Keycloak user UUID")),
    responses(
        (status = 200, description = "User detail", body = Object),
        (status = 404, description = "User not found"),
    ),
    tag = "admin"
)]
pub async fn get_user(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
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

    let mut result = simplify_user(&user);
    result["roles"] = serde_json::json!(roles);
    Ok(Json(result))
}

/// Update a Keycloak user (partial JSON merge). Requires `require_admin`.
#[utoipa::path(
    put,
    path = "/users/{id}",
    params(("id" = String, Path, description = "Keycloak user UUID")),
    request_body(content = Object, description = "Partial user fields to update"),
    responses(
        (status = 200, description = "User updated"),
        (status = 404, description = "User not found"),
    ),
    tag = "admin"
)]
pub async fn update_user(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<serde_json::Value>,
) -> AppResult<Json<serde_json::Value>> {
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

    let enabled = current["enabled"].as_bool().unwrap_or(true);
    revoke_telegram_access(&state, &id, enabled && has_riverdata_role(&roles)).await;

    let mut result = simplify_user(&current);
    result["roles"] = serde_json::json!(roles);
    Ok(Json(result))
}

/// Delete a Keycloak user. Requires `require_admin`.
#[utoipa::path(
    delete,
    path = "/users/{id}",
    params(("id" = String, Path, description = "Keycloak user UUID")),
    responses(
        (status = 200, description = "User deleted"),
        (status = 404, description = "User not found"),
    ),
    tag = "admin"
)]
pub async fn delete_user(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
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

    revoke_telegram_access(&state, &id, false).await;

    Ok(Json(serde_json::json!({ "id": id })))
}

/// Set the realm roles for a user (overwrites; not additive). Requires `require_admin`.
#[utoipa::path(
    post,
    path = "/users/{id}/roles",
    params(("id" = String, Path, description = "Keycloak user UUID")),
    request_body = AssignRolesRequest,
    responses(
        (status = 200, description = "Roles updated"),
        (status = 404, description = "User not found"),
    ),
    tag = "admin"
)]
pub async fn assign_roles(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<AssignRolesRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let token = get_admin_token(&state).await?;
    let client = admin_client(&state)?;
    let base = admin_base_url(&state)?;

    set_user_roles(client, &token, &base, &id, &req.roles).await?;

    revoke_telegram_access(&state, &id, has_riverdata_role(&req.roles)).await;

    Ok(Json(serde_json::json!({ "success": true })))
}

/// List the Keycloak riverdata access roles (`riverdata-admin` / `-manager` / `-river` / `-intern`).
/// Used by the UI's role-assignment picker. Requires `require_admin`.
#[utoipa::path(
    get,
    path = "/roles",
    responses(
        (status = 200, description = "Realm roles", body = [KeycloakRole]),
    ),
    tag = "admin"
)]
pub async fn list_roles(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<serde_json::Value>>> {
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

async fn fetch_role_users(
    client: &KeycloakAdmin,
    token: &str,
    base: &str,
    role_name: &str,
) -> AppResult<Vec<serde_json::Value>> {
    let url = format!("{base}/roles/{role_name}/users");
    tracing::debug!("Fetching role users from: {url}");
    let resp = client
        .http_client
        .get(&url)
        .bearer_auth(token)
        .query(&[("first", "0"), ("max", "1000")])
        .send()
        .await
        .map_err(|e| {
            tracing::warn!("Keycloak role users request error: {e}");
            AppError::Internal(format!("Keycloak role users request failed: {e}"))
        })?;

    // A role that doesn't exist yet (the new intern/river/manager levels before they are created
    // in Keycloak) is not an error — it simply has no members. Any other failure propagates.
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Vec::new());
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!("Keycloak role users request failed ({status}): {body}");
        return Err(AppError::Internal(format!(
            "Keycloak role users request failed ({status}): {body}"
        )));
    }

    let users: Vec<serde_json::Value> = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to parse role users: {e}")))?;
    tracing::debug!("Got {} users with role {role_name}", users.len());
    Ok(users)
}

/// A user's directly assigned riverdata access roles. Only the four canonical levels are
/// returned so every endpoint reports the same `roles` shape as `list_users`.
async fn fetch_user_roles(
    client: &KeycloakAdmin,
    token: &str,
    base: &str,
    user_id: &str,
) -> AppResult<Vec<String>> {
    let resp = client
        .http_client
        .get(format!("{base}/users/{user_id}/role-mappings/realm"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Keycloak role mappings request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "Keycloak role mappings request failed ({status}): {body}"
        )));
    }

    let roles: Vec<KeycloakRole> = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to parse role mappings: {e}")))?;
    Ok(roles
        .into_iter()
        .map(|r| r.name)
        .filter(|n| RIVER_ROLE_NAMES.contains(&n.as_str()))
        .collect())
}

/// Reject role assignments that name a role the realm does not have, or a role that is not one of
/// the river access levels. Checked before any mapping is removed.
fn validate_requested_roles(role_names: &[String], all_roles: &[KeycloakRole]) -> AppResult<()> {
    let join = |names: Vec<&String>| {
        names.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
    };
    let outside: Vec<&String> = role_names
        .iter()
        .filter(|name| !RIVER_ROLE_NAMES.contains(&name.as_str()))
        .collect();
    if !outside.is_empty() {
        return Err(AppError::BadRequest(format!(
            "Not a river access role: {}",
            join(outside)
        )));
    }
    let unknown: Vec<&String> = role_names
        .iter()
        .filter(|name| !all_roles.iter().any(|r| &r.name == *name))
        .collect();
    if !unknown.is_empty() {
        return Err(AppError::BadRequest(format!(
            "Unknown realm role(s): {}",
            join(unknown)
        )));
    }
    Ok(())
}

async fn set_user_roles(
    client: &KeycloakAdmin,
    token: &str,
    base: &str,
    user_id: &str,
    role_names: &[String],
) -> AppResult<()> {
    // Get all realm roles to map names to full representations
    let all_roles_resp = client
        .http_client
        .get(format!("{base}/roles"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to fetch roles: {e}")))?;

    if !all_roles_resp.status().is_success() {
        let status = all_roles_resp.status();
        let body = all_roles_resp.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "Failed to fetch roles ({status}): {body}"
        )));
    }
    let all_roles: Vec<KeycloakRole> = all_roles_resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to parse roles: {e}")))?;

    validate_requested_roles(role_names, &all_roles)?;

    // Remove current realm role mappings
    let current_resp = client
        .http_client
        .get(format!("{base}/users/{user_id}/role-mappings/realm"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to fetch current roles: {e}")))?;

    if !current_resp.status().is_success() {
        let status = current_resp.status();
        let body = current_resp.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "Failed to fetch current roles ({status}): {body}"
        )));
    }
    let current_roles: Vec<KeycloakRole> = current_resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to parse current roles: {e}")))?;

    // Only river access levels are removable; roles granted for other applications stay.
    let removable: Vec<&KeycloakRole> = current_roles
        .iter()
        .filter(|r| RIVER_ROLE_NAMES.contains(&r.name.as_str()))
        .collect();

    if !removable.is_empty() {
        let resp = client
            .http_client
            .delete(format!(
                "{base}/users/{user_id}/role-mappings/realm"
            ))
            .bearer_auth(token)
            .json(&removable)
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("Failed to remove roles: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AppError::Internal(format!(
                "Failed to remove roles ({status}): {body}"
            )));
        }
    }

    // Assign requested roles
    let to_assign: Vec<&KeycloakRole> = all_roles
        .iter()
        .filter(|r| role_names.contains(&r.name))
        .collect();

    if !to_assign.is_empty() {
        let resp = client
            .http_client
            .post(format!(
                "{base}/users/{user_id}/role-mappings/realm"
            ))
            .bearer_auth(token)
            .json(&to_assign)
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("Failed to assign roles: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AppError::Internal(format!(
                "Failed to assign roles ({status}): {body}"
            )));
        }
    }

    Ok(())
}

/// Replace the project visibility grants for a user (`user_project_grants`). Body is the full new
/// set — this overwrites, not appends — mirroring `assign_roles`. Requires `require_admin`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetGrantsRequest {
    /// The complete set of project ids the user may see. An empty array revokes all access.
    pub project_ids: Vec<uuid::Uuid>,
}

/// List the projects a user is granted, with names. Administrators are unrestricted (they are never
/// granted rows); this reflects only the stored grant set. Requires `require_admin`.
pub async fn list_user_grants(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Vec<crate::routes::private::me::GrantedProject>>> {
    let rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT p.id, p.name FROM user_project_grants g \
             JOIN projects p ON p.id = g.project_id \
             WHERE g.user_sub = $1 ORDER BY p.name",
            [id.into()],
        ))
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let grants = rows
        .iter()
        .filter_map(|r| {
            Some(crate::routes::private::me::GrantedProject {
                project_id: r.try_get::<uuid::Uuid>("", "id").ok()?,
                name: r.try_get::<String>("", "name").ok()?,
            })
        })
        .collect();
    Ok(Json(grants))
}

/// Replace a user's project grants transactionally and bust their grants cache so the change takes
/// effect within one request. Requires `require_admin`.
pub async fn set_user_grants(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Path(id): Path<String>,
    Json(req): Json<SetGrantsRequest>,
) -> AppResult<Json<serde_json::Value>> {
    use sea_orm::TransactionTrait;
    let granted_by = auth.keycloak_sub().unwrap_or("").to_string();
    let txn = state
        .db
        .begin()
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "DELETE FROM user_project_grants WHERE user_sub = $1",
        [id.clone().into()],
    ))
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;
    for project_id in &req.project_ids {
        txn.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO user_project_grants (user_sub, project_id, granted_by) VALUES ($1, $2, $3) \
             ON CONFLICT (user_sub, project_id) DO NOTHING",
            [id.clone().into(), (*project_id).into(), granted_by.clone().into()],
        ))
        .await
        .map_err(|e| AppError::BadRequest(format!("grant insert failed (unknown project?): {e}")))?;
    }
    txn.commit().await.map_err(|e| AppError::Internal(e.to_string()))?;

    state.grants_cache.invalidate(&id).await;

    Ok(Json(serde_json::json!({ "success": true, "count": req.project_ids.len() })))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_users))
        .route("/search", get(search_users))
        .route("/{id}", get(get_user).put(update_user).delete(delete_user))
        .route("/{id}/roles", axum::routing::post(assign_roles))
        .route("/{id}/grants", get(list_user_grants).put(set_user_grants))
}
