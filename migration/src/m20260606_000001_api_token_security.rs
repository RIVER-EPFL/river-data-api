use sea_orm_migration::prelude::*;

/// Hardens `api_tokens` for the secure external-push key feature:
/// - `token_prefix` (indexed, unique) — the non-secret lookup key carved out of the token
///   string `rvd_<prefix>_<secret>`. Argon2 hashes are salted and cannot be queried, so the
///   prefix is what we look up; the secret is then argon2-verified.
/// - `description` — per-key allocation label (which client/logger the key belongs to).
/// - `rate_limit_per_second` — optional per-token request ceiling (NULL = unlimited).
///
/// Existing rows are cleared: token creation never worked end-to-end through the UI (wrong path
/// and wrong response field), so there is no real install base to preserve, and legacy SHA-256
/// hashes are incompatible with the new argon2 + prefix scheme.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DELETE FROM api_tokens").await?;
        db.execute_unprepared("ALTER TABLE api_tokens ADD COLUMN IF NOT EXISTS token_prefix TEXT")
            .await?;
        // Table is now empty and every code path mints a prefix, so enforce NOT NULL for integrity.
        db.execute_unprepared("ALTER TABLE api_tokens ALTER COLUMN token_prefix SET NOT NULL")
            .await?;
        db.execute_unprepared("ALTER TABLE api_tokens ADD COLUMN IF NOT EXISTS description TEXT")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE api_tokens ADD COLUMN IF NOT EXISTS rate_limit_per_second INTEGER",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_api_tokens_token_prefix \
             ON api_tokens (token_prefix)",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_api_tokens_token_prefix")
            .await?;
        db.execute_unprepared("ALTER TABLE api_tokens DROP COLUMN IF EXISTS rate_limit_per_second")
            .await?;
        db.execute_unprepared("ALTER TABLE api_tokens DROP COLUMN IF EXISTS description")
            .await?;
        db.execute_unprepared("ALTER TABLE api_tokens DROP COLUMN IF EXISTS token_prefix")
            .await?;
        Ok(())
    }
}
