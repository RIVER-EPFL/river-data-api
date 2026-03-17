use chrono::Utc;
use moka::future::Cache;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use sha2::{Digest, Sha256};
use std::time::Duration;

use crate::entity::api_tokens;

/// Cache of validated API tokens. Key: token_hash, Value: token model.
/// TTL of 60 seconds — short enough that revocations take effect quickly,
/// long enough to avoid hitting the DB on every request.
pub type TokenCache = Cache<String, api_tokens::Model>;

/// Create a new token validation cache.
#[must_use]
pub fn new_token_cache() -> TokenCache {
    Cache::builder()
        .max_capacity(1000)
        .time_to_live(Duration::from_secs(60))
        .build()
}

/// Hash a raw API token using SHA-256 and return the hex string.
#[must_use]
pub fn hash_token(raw_token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw_token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Generate a cryptographically random API token (64 hex characters).
#[must_use]
pub fn generate_token() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Validate a Bearer token from a request. Returns the token model if valid.
/// Uses an in-memory cache (60s TTL) to avoid hitting the database on every request.
pub async fn validate_bearer_token(
    db: &DatabaseConnection,
    authorization: &str,
    cache: &TokenCache,
) -> Option<api_tokens::Model> {
    let raw_token = authorization.strip_prefix("Bearer ")?.trim();
    if raw_token.is_empty() {
        return None;
    }

    let token_hash = hash_token(raw_token);

    // Check cache first
    if let Some(cached) = cache.get(&token_hash).await {
        // Re-check expiry on cached token
        if let Some(expires_at) = cached.expires_at {
            if expires_at.with_timezone(&Utc) < Utc::now() {
                cache.invalidate(&token_hash).await;
                return None;
            }
        }

        // Fire-and-forget: update last_used_at
        let db = db.clone();
        let token_id = cached.id;
        tokio::spawn(async move {
            let update = api_tokens::ActiveModel {
                id: Set(token_id),
                last_used_at: Set(Some(Utc::now())),
                ..Default::default()
            };
            let _ = api_tokens::Entity::update(update).exec(&db).await;
        });

        return Some(cached);
    }

    // Cache miss — query DB
    let token = api_tokens::Entity::find()
        .filter(api_tokens::Column::TokenHash.eq(&token_hash))
        .filter(api_tokens::Column::IsActive.eq(true))
        .one(db)
        .await
        .ok()??;

    // Check expiry
    if let Some(expires_at) = token.expires_at
        && expires_at.with_timezone(&Utc) < Utc::now()
    {
        return None;
    }

    // Insert into cache
    cache.insert(token_hash, token.clone()).await;

    // Fire-and-forget: update last_used_at
    let db = db.clone();
    let token_id = token.id;
    tokio::spawn(async move {
        let update = api_tokens::ActiveModel {
            id: Set(token_id),
            last_used_at: Set(Some(Utc::now())),
            ..Default::default()
        };
        let _ = api_tokens::Entity::update(update).exec(&db).await;
    });

    Some(token)
}
