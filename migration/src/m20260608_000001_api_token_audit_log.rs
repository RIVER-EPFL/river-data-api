use sea_orm_migration::prelude::*;

/// Forensic trail of API-token usage for the public-facing key surface. Each authenticated request
/// made with an `rvd_` token records (token, method, path, status, the token's project scope, time).
///
/// Deliberately has **no** foreign key to `api_tokens`: the trail must survive token deletion so an
/// incident investigation can still see what a since-removed key accessed. Writes are best-effort
/// and fire-and-forget on the request path (gated by `AUDIT_API_TOKEN_USE`), so this table tolerates
/// gaps and is never on the critical path of a request.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS api_token_audit_log (
                 id UUID PRIMARY KEY,
                 token_id UUID NOT NULL,
                 method TEXT NOT NULL,
                 path TEXT NOT NULL,
                 status_code INTEGER NOT NULL,
                 project_scope UUID,
                 created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
             )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_api_token_audit_token_ts \
             ON api_token_audit_log (token_id, created_at DESC)",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP TABLE IF EXISTS api_token_audit_log")
            .await?;
        Ok(())
    }
}
