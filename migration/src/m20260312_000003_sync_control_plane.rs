use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260312_000003_sync_control_plane"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // 1. sync_services — service registry
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS sync_services (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                service_type TEXT NOT NULL,
                instance_id TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'starting',
                current_operation TEXT,
                last_heartbeat TIMESTAMPTZ,
                last_sync_completed_at TIMESTAMPTZ,
                last_error TEXT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                UNIQUE(service_type, instance_id)
            )"
        ).await?;

        // 2. sync_commands — command queue
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS sync_commands (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                service_id UUID NOT NULL REFERENCES sync_services(id) ON DELETE CASCADE,
                command TEXT NOT NULL,
                payload JSONB,
                status TEXT NOT NULL DEFAULT 'pending',
                result JSONB,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                expires_at TIMESTAMPTZ NOT NULL DEFAULT NOW() + INTERVAL '5 minutes',
                acknowledged_at TIMESTAMPTZ,
                completed_at TIMESTAMPTZ
            )"
        ).await?;

        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_sync_commands_pending
             ON sync_commands(service_id, status) WHERE status = 'pending'"
        ).await?;

        // 3. sync_service_credentials — client credentials for service auth
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS sync_service_credentials (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                client_id TEXT NOT NULL UNIQUE,
                client_secret_hash TEXT NOT NULL,
                service_type TEXT NOT NULL,
                service_id UUID REFERENCES sync_services(id),
                revoked BOOLEAN NOT NULL DEFAULT FALSE,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )"
        ).await?;

        // 4. sync_service_tokens — short-lived session tokens
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS sync_service_tokens (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                service_id UUID NOT NULL REFERENCES sync_services(id) ON DELETE CASCADE,
                token_hash TEXT NOT NULL,
                expires_at TIMESTAMPTZ NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )"
        ).await?;

        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_sync_service_tokens_lookup
             ON sync_service_tokens(token_hash)"
        ).await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared("DROP TABLE IF EXISTS sync_service_tokens CASCADE").await?;
        db.execute_unprepared("DROP TABLE IF EXISTS sync_service_credentials CASCADE").await?;
        db.execute_unprepared("DROP TABLE IF EXISTS sync_commands CASCADE").await?;
        db.execute_unprepared("DROP TABLE IF EXISTS sync_services CASCADE").await?;

        Ok(())
    }
}
