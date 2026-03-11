use chrono::Utc;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use sha2::{Digest, Sha256};

use crate::entity::api_tokens;

/// Hash a raw API token using SHA-256 and return the hex string.
pub fn hash_token(raw_token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw_token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Generate a cryptographically random API token (64 hex characters).
pub fn generate_token() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Validate a Bearer token from a request. Returns the token model if valid.
pub async fn validate_bearer_token(
    db: &DatabaseConnection,
    authorization: &str,
) -> Option<api_tokens::Model> {
    let raw_token = authorization.strip_prefix("Bearer ")?.trim();
    if raw_token.is_empty() {
        return None;
    }

    let token_hash = hash_token(raw_token);

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
