//! Minting, hashing and verifying a bearer token, and the Keycloak admin client the user routes
//! read and write the realm through.

use std::time::Duration;

use argon2::Algorithm;
use argon2::Argon2;
use argon2::Params;
use argon2::Version;
use argon2::password_hash::PasswordHash;
use argon2::password_hash::PasswordHasher;
use argon2::password_hash::PasswordVerifier;
use argon2::password_hash::SaltString;
use argon2::password_hash::rand_core::OsRng;
use chrono::Utc;
use crudcrate::ApiError;
use crudcrate::CRUDOperations;
use crudcrate::CRUDResource;
use moka::future::Cache;
use rand::Rng;
use sea_orm::ActiveModelTrait;
use sea_orm::ColumnTrait;
use sea_orm::ConnectionTrait;
use sea_orm::DatabaseConnection;
use sea_orm::EntityTrait;
use sea_orm::QueryFilter;
use sea_orm::Set;
use sea_orm::TransactionTrait;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;

use super::models as model;
use super::models::ApiToken;
use super::models::*;
use crate::common::AppState;
use crate::common::authz::RIVER_ROLE_NAMES;
use crate::common::state::KeycloakAdmin;
use crate::error::AppError;
use crate::error::AppResult;

/// Cache of validated API tokens. Key: SHA-256 of the raw bearer token (in-memory only, never
/// stored), Value: token model. Short TTL so expirations take effect quickly; revocation/rotation
/// busts the whole cache explicitly (see `invalidate_token_cache`).
pub type TokenCache = Cache<String, model::Model>;

/// Default TTL for the token validation cache when none is configured. Short by design: expiry is
/// re-checked on every cache hit and revoke/rotate bust the whole cache, so a small TTL keeps the
/// window for any out-of-band `is_active` flip tight at negligible DB cost.
pub const DEFAULT_TOKEN_CACHE_TTL_SECONDS: u64 = 5;

/// Create a new token validation cache with the given TTL (seconds).
#[must_use]
pub fn new_token_cache(ttl_seconds: u64) -> TokenCache {
    let ttl = if ttl_seconds == 0 {
        DEFAULT_TOKEN_CACHE_TTL_SECONDS
    } else {
        ttl_seconds
    };
    Cache::builder()
        .max_capacity(1000)
        .time_to_live(Duration::from_secs(ttl))
        .build()
}

/// Drop every cached validation. Called when a token is revoked, rotated, or deleted so the
/// change takes effect on the very next request instead of waiting out the TTL. Token mutations
/// are rare admin actions, so clearing the whole (≤1000-entry) cache is cheap and avoids having
/// to map a token id back to its (unknown) raw-token cache key.
pub async fn invalidate_token_cache(cache: &TokenCache) {
    cache.invalidate_all();
}

/// SHA-256 hex of an input. Used (a) as the in-memory cache key for API tokens and (b) as the
/// deterministic lookup hash for **sync service session tokens**, which are looked up by exact
/// equality and are a separate system from the argon2-hashed API tokens below.
#[must_use]
pub fn hash_token(raw_token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw_token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// The public prefix of every API token string: `rvd_<prefix>_<secret>`.
const API_TOKEN_PREFIX: &str = "rvd_";

/// Hex length of the non-secret lookup prefix produced by [`mint_api_token`] (8 bytes → 16 chars).
const PREFIX_HEX_LEN: usize = 16;
/// Hex length of the secret produced by [`mint_api_token`] (32 bytes → 64 chars).
const SECRET_HEX_LEN: usize = 64;

/// Whether every byte of `s` is a lowercase hex digit. Mirrors [`random_hex`]'s `{:02x}` output.
fn is_lower_hex(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A freshly minted API token. `raw_token` is shown to the operator exactly once; only
/// `token_prefix` (indexed lookup key, non-secret) and `token_hash` (argon2id of the secret)
/// are persisted.
pub struct MintedToken {
    pub raw_token: String,
    pub token_prefix: String,
    pub token_hash: String,
}

fn random_hex(n_bytes: usize) -> String {
    let mut rng = rand::rng();
    (0..n_bytes)
        .map(|_| format!("{:02x}", rng.random::<u8>()))
        .collect()
}

/// Mint a new API token: `rvd_<16-hex-prefix>_<64-hex-secret>`. The prefix (64 bits) is the
/// non-secret indexed lookup key; the secret (256 bits) is argon2id-hashed for storage. Both the
/// real create endpoint and the test seed helpers go through this so the on-the-wire format and
/// the at-rest format never drift apart.
#[must_use]
pub fn mint_api_token() -> MintedToken {
    let prefix = random_hex(8); // 16 hex chars (64 bits), non-secret lookup key
    let secret = random_hex(32); // 64 hex chars (256 bits)
    let raw_token = format!("{API_TOKEN_PREFIX}{prefix}_{secret}");
    let token_hash = hash_api_secret(&secret);
    MintedToken {
        raw_token,
        token_prefix: prefix,
        token_hash,
    }
}

/// Split a raw API token `rvd_<prefix>_<secret>` into its lookup prefix and secret parts.
/// Returns `None` for anything that isn't a well-formed API token (e.g. a Keycloak JWT or a sync
/// session token), so the caller can fall through to the next auth method.
#[must_use]
pub fn split_api_token(raw_token: &str) -> Option<(&str, &str)> {
    let rest = raw_token.strip_prefix(API_TOKEN_PREFIX)?;
    let (prefix, secret) = rest.split_once('_')?;
    // Reject anything not shaped exactly like a minted token: 16-hex prefix, 64-hex secret. This
    // rejects malformed `rvd_…` junk before it can reach the indexed prefix lookup, so a flood of
    // well-prefixed-but-bogus bearer values can't drive DB work (the per-IP limiter runs ahead of
    // this; the length/hex gate is the second line). A wrong-but-well-formed secret still verifies
    // against argon2, only the at-rest hash can reject that.
    if prefix.len() != PREFIX_HEX_LEN
        || secret.len() != SECRET_HEX_LEN
        || !is_lower_hex(prefix)
        || !is_lower_hex(secret)
    {
        return None;
    }
    Some((prefix, secret))
}

/// Argon2id with explicit OWASP-baseline parameters (m = 19 MiB, t = 2, p = 1). Pinned rather than
/// using `Argon2::default()` so a future change to the crate's defaults can't silently weaken the
/// work factor, or raise it enough to turn cache-miss verification into a CPU-DoS vector.
fn token_argon2() -> Argon2<'static> {
    let params = Params::new(19_456, 2, 1, None).expect("static argon2 params are valid");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Argon2id hash (PHC string) of a token secret. Argon2 is salted, so two identical secrets
/// produce different hashes, that's why lookup is by `token_prefix`, not by this value.
#[must_use]
pub fn hash_api_secret(secret: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    token_argon2()
        .hash_password(secret.as_bytes(), &salt)
        .expect("argon2 hashing of a token secret cannot fail")
        .to_string()
}

/// Constant-time verification of a token secret against its stored argon2 PHC hash. Argon2's
/// verifier compares in constant time, so no separate timing-safe compare is needed on the hot
/// path. Returns `false` on any malformed stored hash rather than leaking a parse error.
#[must_use]
pub fn verify_api_secret(secret: &str, phc: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    // The cost parameters used for verification come from the stored PHC string, so this validates
    // any historical hash; `token_argon2()` only fixes the params used when minting new hashes.
    token_argon2()
        .verify_password(secret.as_bytes(), &parsed)
        .is_ok()
}

/// Validate a Bearer token from a request. Returns the token model if valid (active, unexpired,
/// secret verifies). Uses the in-memory cache (keyed by SHA-256 of the raw token) to avoid an
/// argon2 verification on every request; a cache miss does the indexed prefix lookup + verify.
pub async fn validate_bearer_token(
    db: &DatabaseConnection,
    authorization: &str,
    cache: &TokenCache,
) -> Option<model::Model> {
    let raw_token = authorization.strip_prefix("Bearer ")?.trim();
    if raw_token.is_empty() {
        return None;
    }

    // Fast-fail on anything that isn't a well-formed `rvd_<prefix>_<secret>` API token (a Keycloak
    // JWT or sync session token). This bails before touching the cache, the DB, or argon2, so a
    // flood of malformed bearer values can't drive any work on this path.
    split_api_token(raw_token)?;

    let cache_key = hash_token(raw_token);

    // Cache hit: the key is the SHA-256 of the exact raw token, so a wrong secret can never hit a
    // cached entry. Still re-check expiry (it's time-dependent); is_active changes bust the cache.
    if let Some(cached) = cache.get(&cache_key).await {
        if is_expired(&cached) {
            cache.invalidate(&cache_key).await;
            return None;
        }
        touch_last_used(db, cached.id);
        return Some(cached);
    }

    // Cache miss: parse the token, look up the row by its (indexed, unique) prefix, then verify
    // the secret in constant time with argon2.
    let (prefix, secret) = split_api_token(raw_token)?;

    let token = model::Entity::find()
        .filter(model::Column::TokenPrefix.eq(prefix))
        .filter(model::Column::IsActive.eq(true))
        .one(db)
        .await
        .ok()??;

    if !verify_api_secret(secret, &token.token_hash) {
        return None;
    }
    if is_expired(&token) {
        return None;
    }

    cache.insert(cache_key, token.clone()).await;
    touch_last_used(db, token.id);
    Some(token)
}

fn is_expired(token: &model::Model) -> bool {
    token
        .expires_at
        .is_some_and(|e| e.with_timezone(&Utc) < Utc::now())
}

/// Append to the API-token audit log (forensic trail for the public key surface). Spawned off the
/// request path so it never blocks or fails a request; a failed append is logged at `warn`, since
/// a trail that stops growing silently is the one failure the trail exists to catch. Captures the
/// token, the request method+path, the response status, and the token's project scope, including
/// the 403s a scoped key earns on a cross-project attempt.
pub fn record_token_use(
    db: &DatabaseConnection,
    token_id: uuid::Uuid,
    scope: Option<uuid::Uuid>,
    method: &str,
    path: &str,
    status: u16,
) {
    let db = db.clone();
    let row = super::audit_log::ActiveModel {
        id: Set(uuid::Uuid::new_v4()),
        token_id: Set(token_id),
        method: Set(method.to_string()),
        path: Set(path.to_string()),
        status_code: Set(i32::from(status)),
        project_scope: Set(scope),
        created_at: Set(Utc::now().into()),
    };
    tokio::spawn(async move {
        if let Err(e) = row.insert(&db).await {
            tracing::warn!(error = %e, token_id = %token_id, "Failed to append api token audit log");
        }
    });
}

/// Fire-and-forget `last_used_at` bump for audit. Best-effort; failures are ignored.
fn touch_last_used(db: &DatabaseConnection, token_id: uuid::Uuid) {
    let db = db.clone();
    tokio::spawn(async move {
        let update = model::ActiveModel {
            id: Set(token_id),
            last_used_at: Set(Some(Utc::now())),
            ..Default::default()
        };
        let _ = model::Entity::update(update).exec(&db).await;
    });
}

pub struct ApiTokenOperations;

impl CRUDOperations for ApiTokenOperations {
    type Resource = ApiToken;

    async fn before_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        _db: &C,
        _id: uuid::Uuid,
        data: &<ApiToken as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        crate::common::actor::refuse_reattribution(data.created_by.is_some())
    }

    async fn perform_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        data: <ApiToken as CRUDResource>::CreateModel,
    ) -> Result<ApiToken, ApiError> {
        let minted = mint_api_token();

        let mut active_model: model::ActiveModel = data.into();
        active_model.token_hash = Set(minted.token_hash);
        active_model.token_prefix = Set(minted.token_prefix);

        let model = active_model.insert(db).await.map_err(ApiError::database)?;
        let mut token = ApiToken::from(model);
        // The raw secret is returned exactly once, here, and never persisted.
        token.token = Some(minted.raw_token);
        Ok(token)
    }
}

// --- The Keycloak admin client ---
//
// The realm is the directory; river-data stores no user row. These wrap the admin REST API the
// user routes read and write through.

/// Anti-backdoor hook: a user's cached access must not outlive their real access. On any change to
/// a user's roles, enabled flag or existence, drop their cached role and their cached project
/// grants so both re-resolve on the next request rather than waiting for a sweep.
pub(crate) async fn invalidate_cached_access(state: &AppState, sub: &str) {
    state.authorizer.invalidate(sub).await;
    state.grants_cache.invalidate(sub).await;
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
            && *expiry > Utc::now() + chrono::Duration::seconds(30)
        {
            return Ok(token.clone());
        }
    }

    let url = format!(
        "{}/realms/{}/protocol/openid-connect/token",
        state
            .config
            .keycloak_url
            .as_ref()
            .ok_or_else(|| AppError::ServiceUnavailable("Keycloak not configured".to_string()))?,
        state
            .config
            .keycloak_realm
            .as_ref()
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
        state
            .config
            .keycloak_url
            .as_ref()
            .ok_or_else(|| AppError::ServiceUnavailable("Keycloak not configured".to_string()))?,
        state
            .config
            .keycloak_realm
            .as_ref()
            .ok_or_else(|| AppError::ServiceUnavailable("Keycloak not configured".to_string()))?,
    ))
}

pub(crate) fn admin_client(state: &AppState) -> AppResult<&KeycloakAdmin> {
    state
        .keycloak_admin
        .as_ref()
        .ok_or_else(|| AppError::ServiceUnavailable("Keycloak not configured".to_string()))
}

/// Transform a Keycloak user JSON into our simplified format.
pub(crate) fn simplify_user(u: &serde_json::Value, roles: Vec<String>) -> KeycloakUser {
    KeycloakUser {
        id: u["id"].as_str().map(str::to_string),
        username: u["username"].as_str().map(str::to_string),
        email: u["email"].as_str().map(str::to_string),
        first_name: u["firstName"].as_str().map(str::to_string),
        last_name: u["lastName"].as_str().map(str::to_string),
        enabled: u["enabled"].as_bool(),
        created_timestamp: u["createdTimestamp"].as_i64(),
        roles,
    }
}

/// The `RIVER_ROLE_NAMES` absent from a realm's role list.
fn missing_role_names(present: &[String]) -> Vec<&'static str> {
    RIVER_ROLE_NAMES
        .into_iter()
        .filter(|want| !present.iter().any(|have| have == want))
        .collect()
}

/// Ask the realm which roles exist. Every level in `RIVER_ROLE_NAMES` must be present, otherwise
/// users of that level authenticate but resolve to level 0 and are refused at the door, and the
/// role picker silently offers fewer levels than the authorization matrix defines. Neither
/// failure is visible from inside a running API, so it is checked once at startup.
pub async fn check_realm_roles(state: &AppState) -> RealmRoleCheck {
    let token = match get_admin_token(state).await {
        Ok(t) => t,
        Err(e) => return RealmRoleCheck::Unavailable(format!("admin token request failed: {e}")),
    };
    let (client, base) = match (admin_client(state), admin_base_url(state)) {
        (Ok(c), Ok(b)) => (c, b),
        _ => return RealmRoleCheck::Unavailable("Keycloak admin not configured".to_string()),
    };

    let resp = match client
        .http_client
        .get(format!("{base}/roles"))
        .bearer_auth(&token)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return RealmRoleCheck::Unavailable(format!("realm roles request failed: {e}")),
    };

    // A 403 here means the service account lacks `view-realm`. That is a misconfiguration rather
    // than an outage, but it is reported as unavailable because the roles themselves are unknown:
    // refusing to start on an unverifiable realm is the same call either way.
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return RealmRoleCheck::Unavailable(format!(
            "realm roles request failed ({status}): {body}"
        ));
    }

    let roles: Vec<KeycloakRole> = match resp.json().await {
        Ok(r) => r,
        Err(e) => return RealmRoleCheck::Unavailable(format!("failed to parse realm roles: {e}")),
    };

    let present: Vec<String> = roles.into_iter().map(|r| r.name).collect();
    match missing_role_names(&present) {
        m if m.is_empty() => RealmRoleCheck::Satisfied,
        m => RealmRoleCheck::Missing(m),
    }
}

/// Page size for the Keycloak admin listings; every listing here is walked to its end.
const KC_PAGE: usize = 100;

/// GET a Keycloak admin listing page by page until a short page. A 404 is an empty listing (a
/// role or endpoint the realm does not have); any other failure propagates.
async fn fetch_all_pages(
    client: &KeycloakAdmin,
    token: &str,
    url: &str,
    what: &str,
) -> AppResult<Vec<serde_json::Value>> {
    let mut out = Vec::new();
    let mut first = 0usize;
    loop {
        let resp = client
            .http_client
            .get(url)
            .bearer_auth(token)
            .query(&[("first", first.to_string()), ("max", KC_PAGE.to_string())])
            .send()
            .await
            .map_err(|e| {
                tracing::warn!("Keycloak {what} request error: {e}");
                AppError::Internal(format!("Keycloak {what} request failed: {e}"))
            })?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(out);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!("Keycloak {what} request failed ({status}): {body}");
            return Err(AppError::Internal(format!(
                "Keycloak {what} request failed ({status}): {body}"
            )));
        }
        let page: Vec<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| AppError::Internal(format!("Failed to parse {what}: {e}")))?;
        let n = page.len();
        out.extend(page);
        if n < KC_PAGE {
            return Ok(out);
        }
        first += n;
    }
}

/// The users directly mapped to a realm role, every page of them.
async fn fetch_role_users(
    client: &KeycloakAdmin,
    token: &str,
    base: &str,
    role_name: &str,
) -> AppResult<Vec<serde_json::Value>> {
    let users = fetch_all_pages(
        client,
        token,
        &format!("{base}/roles/{role_name}/users"),
        "role users",
    )
    .await?;
    tracing::debug!("Got {} users with role {role_name}", users.len());
    Ok(users)
}

fn river_role_names(roles: &[serde_json::Value]) -> Vec<String> {
    roles
        .iter()
        .filter_map(|r| r["name"].as_str())
        .filter(|n| RIVER_ROLE_NAMES.contains(n))
        .map(str::to_string)
        .collect()
}

/// `(river role, members)` from every path a realm role reaches a user: a direct mapping, a
/// composite role that contains it, and a group (or an ancestor group) that maps it. Access is
/// decided from the JWT, which carries all three, so the list has to see all three.
pub(crate) async fn effective_role_members(
    client: &KeycloakAdmin,
    token: &str,
    base: &str,
) -> AppResult<Vec<(String, Vec<serde_json::Value>)>> {
    let mut out = Vec::new();
    let direct = futures::future::join_all(
        RIVER_ROLE_NAMES
            .iter()
            .map(|role| fetch_role_users(client, token, base, role)),
    )
    .await;
    for (role, members) in RIVER_ROLE_NAMES.iter().zip(direct) {
        out.push(((*role).to_string(), members?));
    }

    // Composite roles: every realm role whose expansion contains a river level.
    let all_roles = fetch_all_pages(client, token, &format!("{base}/roles"), "roles").await?;
    for role in &all_roles {
        let Some(name) = role["name"].as_str() else {
            continue;
        };
        if role["composite"].as_bool() != Some(true) || RIVER_ROLE_NAMES.contains(&name) {
            continue;
        }
        let expanded = fetch_all_pages(
            client,
            token,
            &format!("{base}/roles/{name}/composites/realm"),
            "role composites",
        )
        .await?;
        let contained = river_role_names(&expanded);
        if contained.is_empty() {
            continue;
        }
        let members = fetch_role_users(client, token, base, name).await?;
        for level in contained {
            out.push((level, members.clone()));
        }
    }

    // Groups: a group's effective realm mappings (composites expanded) plus what it inherits
    // from its ancestors, applied to its direct members; children walked with that inheritance.
    let top = fetch_all_pages(client, token, &format!("{base}/groups"), "groups").await?;
    let mut stack: Vec<(serde_json::Value, Vec<String>)> =
        top.into_iter().map(|g| (g, Vec::new())).collect();
    while let Some((group, inherited)) = stack.pop() {
        let Some(id) = group["id"].as_str() else {
            continue;
        };
        let mapped = fetch_all_pages(
            client,
            token,
            &format!("{base}/groups/{id}/role-mappings/realm/composite"),
            "group role mappings",
        )
        .await?;
        let mut levels = inherited;
        for level in river_role_names(&mapped) {
            if !levels.contains(&level) {
                levels.push(level);
            }
        }
        if !levels.is_empty() {
            let members = fetch_all_pages(
                client,
                token,
                &format!("{base}/groups/{id}/members"),
                "group members",
            )
            .await?;
            for level in &levels {
                out.push((level.clone(), members.clone()));
            }
        }
        let children = fetch_all_pages(
            client,
            token,
            &format!("{base}/groups/{id}/children"),
            "group children",
        )
        .await?;
        stack.extend(children.into_iter().map(|c| (c, levels.clone())));
    }
    Ok(out)
}

/// A user's effective riverdata access roles, composites and group mappings expanded, which is
/// what their JWT carries. Only the four canonical levels are returned so every endpoint reports
/// the same `roles` shape as `list_users`.
pub(crate) async fn fetch_user_roles(
    client: &KeycloakAdmin,
    token: &str,
    base: &str,
    user_id: &str,
) -> AppResult<Vec<String>> {
    let resp = client
        .http_client
        .get(format!(
            "{base}/users/{user_id}/role-mappings/realm/composite"
        ))
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
        names
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ")
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

pub(crate) async fn set_user_roles(
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
            .delete(format!("{base}/users/{user_id}/role-mappings/realm"))
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
            .post(format!("{base}/users/{user_id}/role-mappings/realm"))
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

#[cfg(test)]
#[path = "tests/service.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/keycloak_roles.rs"]
mod keycloak_roles_tests;
