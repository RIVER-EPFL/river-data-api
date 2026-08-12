use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng};
use argon2::{Algorithm, Argon2, Params, Version};
use chrono::Utc;
use moka::future::Cache;
use rand::Rng;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use sha2::{Digest, Sha256};
use std::time::Duration;

use super::model;

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
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
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
    (0..n_bytes).map(|_| format!("{:02x}", rng.random::<u8>())).collect()
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
    MintedToken { raw_token, token_prefix: prefix, token_hash }
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
    if split_api_token(raw_token).is_none() {
        return None;
    }

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

/// Fire-and-forget write to the API-token audit log (forensic trail for the public key surface).
/// Best-effort: spawned off the request path and errors are ignored, so it never blocks or fails a
/// request. Captures the token, the request method+path, the response status, and the token's
/// project scope, including the 403s a scoped key earns on a cross-project attempt.
pub fn record_token_use(
    db: &DatabaseConnection,
    token_id: uuid::Uuid,
    scope: Option<uuid::Uuid>,
    method: &str,
    path: &str,
    status: u16,
) {
    use sea_orm::{ConnectionTrait, Statement};
    let db = db.clone();
    let method = method.to_string();
    let path = path.to_string();
    let scope_val = match scope {
        Some(u) => sea_orm::Value::from(u),
        None => sea_orm::Value::Uuid(None),
    };
    tokio::spawn(async move {
        let _ = db
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "INSERT INTO api_token_audit_log \
                 (id, token_id, method, path, status_code, project_scope) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
                [
                    uuid::Uuid::new_v4().into(),
                    token_id.into(),
                    method.into(),
                    path.into(),
                    i32::from(status).into(),
                    scope_val,
                ],
            ))
            .await;
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
