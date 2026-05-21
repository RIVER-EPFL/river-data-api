use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue},
    response::IntoResponse,
    routing::get,
};
use chrono::Utc;
use serde::Deserialize;

use crate::common::AppState;
use crate::common::auth::Role;
use crate::common::state::KeycloakAdmin;
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub range: Option<String>,
    pub filter: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateUserRequest {
    pub username: String,
    pub email: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub password: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct KeycloakRole {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct AssignRolesRequest {
    roles: Vec<String>,
}

async fn get_admin_token(state: &AppState) -> AppResult<String> {
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

fn admin_base_url(state: &AppState) -> AppResult<String> {
    Ok(format!(
        "{}/admin/realms/{}",
        state.config.keycloak_url.as_ref()
            .ok_or_else(|| AppError::ServiceUnavailable("Keycloak not configured".to_string()))?,
        state.config.keycloak_realm.as_ref()
            .ok_or_else(|| AppError::ServiceUnavailable("Keycloak not configured".to_string()))?,
    ))
}

fn admin_client(state: &AppState) -> AppResult<&KeycloakAdmin> {
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

async fn list_users(
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
        (start, end - start + 1)
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

    // Fetch only users with the riverdata-user role (not the entire realm)
    let user_role = Role::User.to_string();
    let admin_role = Role::Administrator.to_string();
    let kc_users = fetch_role_users(client, &token, &base, &user_role).await;

    // Fetch roles for each user concurrently
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

    let mut users: Vec<serde_json::Value> = kc_users
        .iter()
        .zip(all_roles)
        .map(|(u, roles)| {
            let mut user = simplify_user(u);
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

async fn get_user(
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
    let roles = fetch_user_roles(client, &token, &base, &id).await;

    let mut result = simplify_user(&user);
    result["roles"] = serde_json::json!(roles);
    Ok(Json(result))
}

async fn create_user(
    State(state): State<AppState>,
    Json(req): Json<CreateUserRequest>,
) -> Result<impl IntoResponse, AppError> {
    let token = get_admin_token(&state).await?;
    let client = admin_client(&state)?;
    let base = admin_base_url(&state)?;

    let mut body = serde_json::json!({
        "username": req.username,
        "email": req.email,
        "firstName": req.first_name,
        "lastName": req.last_name,
        "enabled": req.enabled.unwrap_or(true),
    });

    if let Some(password) = &req.password {
        body["credentials"] = serde_json::json!([{
            "type": "password",
            "value": password,
            "temporary": false,
        }]);
    }

    let resp = client
        .http_client
        .post(format!("{base}/users"))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Keycloak request failed: {e}")))?;

    if resp.status() == reqwest::StatusCode::CONFLICT {
        return Err(AppError::BadRequest("User already exists".to_string()));
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "Keycloak create user failed ({status}): {body}"
        )));
    }

    // Extract user ID from Location header
    let user_id = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.rsplit('/').next())
        .map(String::from)
        .ok_or_else(|| AppError::Internal("No user ID in Keycloak response".to_string()))?;

    // Fetch the created user to return it
    let user_resp = client
        .http_client
        .get(format!("{base}/users/{user_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to fetch created user: {e}")))?;

    let user: serde_json::Value = user_resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to parse created user: {e}")))?;

    Ok(Json(simplify_user(&user)))
}

async fn update_user(
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
        fetch_user_roles(client, &token, &base, &id).await
    };

    let mut result = simplify_user(&current);
    result["roles"] = serde_json::json!(roles);
    Ok(Json(result))
}

async fn delete_user(
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

    Ok(Json(serde_json::json!({ "id": id })))
}

async fn assign_roles(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<AssignRolesRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let token = get_admin_token(&state).await?;
    let client = admin_client(&state)?;
    let base = admin_base_url(&state)?;

    set_user_roles(client, &token, &base, &id, &req.roles).await?;

    Ok(Json(serde_json::json!({ "success": true })))
}

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

    let roles: Vec<KeycloakRole> = if resp.status().is_success() {
        resp.json().await.unwrap_or_default()
    } else {
        vec![]
    };

    // Filter out Keycloak internal roles
    let roles: Vec<serde_json::Value> = roles
        .into_iter()
        .filter(|r| {
            !r.name.starts_with("default-roles-")
                && r.name != "uma_authorization"
                && r.name != "offline_access"
        })
        .map(|r| serde_json::json!({ "id": r.id, "name": r.name }))
        .collect();

    Ok(Json(roles))
}

async fn fetch_role_users(
    client: &KeycloakAdmin,
    token: &str,
    base: &str,
    role_name: &str,
) -> Vec<serde_json::Value> {
    let url = format!("{base}/roles/{role_name}/users");
    tracing::debug!("Fetching role users from: {url}");
    let resp = client
        .http_client
        .get(&url)
        .bearer_auth(token)
        .query(&[("first", "0"), ("max", "1000")])
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let users: Vec<serde_json::Value> = r.json().await.unwrap_or_default();
            tracing::debug!("Got {} users with role {role_name}", users.len());
            users
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            tracing::warn!("Keycloak role users request failed ({status}): {body}");
            vec![]
        }
        Err(e) => {
            tracing::warn!("Keycloak role users request error: {e}");
            vec![]
        }
    }
}

async fn fetch_user_roles(
    client: &KeycloakAdmin,
    token: &str,
    base: &str,
    user_id: &str,
) -> Vec<String> {
    let resp = client
        .http_client
        .get(format!("{base}/users/{user_id}/role-mappings/realm"))
        .bearer_auth(token)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let roles: Vec<KeycloakRole> = r.json().await.unwrap_or_default();
            roles
                .into_iter()
                .map(|r| r.name)
                .filter(|n| {
                    !n.starts_with("default-roles-")
                        && n != "uma_authorization"
                        && n != "offline_access"
                })
                .collect()
        }
        _ => vec![],
    }
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

    let all_roles: Vec<KeycloakRole> = all_roles_resp.json().await.unwrap_or_default();

    // Remove current realm role mappings
    let current_resp = client
        .http_client
        .get(format!("{base}/users/{user_id}/role-mappings/realm"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to fetch current roles: {e}")))?;

    let current_roles: Vec<KeycloakRole> = current_resp.json().await.unwrap_or_default();

    // Only remove non-default roles
    let removable: Vec<&KeycloakRole> = current_roles
        .iter()
        .filter(|r| {
            !r.name.starts_with("default-roles-")
                && r.name != "uma_authorization"
                && r.name != "offline_access"
        })
        .collect();

    if !removable.is_empty() {
        client
            .http_client
            .delete(format!(
                "{base}/users/{user_id}/role-mappings/realm"
            ))
            .bearer_auth(token)
            .json(&removable)
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("Failed to remove roles: {e}")))?;
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

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_users).post(create_user))
        .route("/{id}", get(get_user).put(update_user).delete(delete_user))
        .route("/{id}/roles", axum::routing::post(assign_roles))
}
