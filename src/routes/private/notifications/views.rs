//! Admin HTTP handlers for the notification layer (link-code minting).

use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use rand::Rng;
use sea_orm::{ConnectionTrait, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;
const LINK_CODE_TTL_MINUTES: i64 = 60;

#[derive(Debug, Deserialize, ToSchema)]
pub struct GenerateLinkCodeRequest {
    /// Keycloak user `sub` this chat will speak for. The bot's role checks resolve against it.
    pub keycloak_sub: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LinkCodeResponse {
    pub code: String,
    pub expires_at: DateTime<Utc>,
}

/// Avoids visually ambiguous characters (no l/1/i/o/0) so codes are easy to relay.
fn generate_code() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::rng();
    (0..8)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

/// Mint a one-time link code for a Keycloak user. The user sends `/start <code>` to the bot to
/// claim it. Any prior unclaimed code for the same user is dropped, so a user has at most one
/// pending code. Admin-only.
#[utoipa::path(
    post,
    path = "/telegram_identities/link_code",
    request_body = GenerateLinkCodeRequest,
    responses((status = 200, description = "Link code minted", body = LinkCodeResponse)),
    tag = "notifications"
)]
pub async fn generate_link_code(
    State(state): State<AppState>,
    Json(req): Json<GenerateLinkCodeRequest>,
) -> AppResult<Json<LinkCodeResponse>> {
    if req.keycloak_sub.trim().is_empty() {
        return Err(AppError::BadRequest("keycloak_sub is required".to_string()));
    }
    let code = generate_code();

    state
        .db
        .execute(Statement::from_sql_and_values(
            PG,
            "DELETE FROM telegram_identities \
             WHERE linked_keycloak_sub = $1 AND telegram_chat_id IS NULL",
            [req.keycloak_sub.clone().into()],
        ))
        .await
        .map_err(|e| AppError::Internal(format!("failed to clear pending codes: {e}")))?;

    let row = state
        .db
        .query_one(Statement::from_sql_and_values(
            PG,
            "INSERT INTO telegram_identities \
                (linked_keycloak_sub, link_code, link_code_expires_at, is_active) \
             VALUES ($1, $2, NOW() + ($3 || ' minutes')::interval, TRUE) \
             RETURNING link_code_expires_at",
            [
                req.keycloak_sub.into(),
                code.clone().into(),
                LINK_CODE_TTL_MINUTES.to_string().into(),
            ],
        ))
        .await
        .map_err(|e| AppError::Internal(format!("failed to mint link code: {e}")))?
        .ok_or_else(|| AppError::Internal("no row returned minting link code".to_string()))?;

    let expires_at: DateTime<Utc> = row
        .try_get("", "link_code_expires_at")
        .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(LinkCodeResponse { code, expires_at }))
}
