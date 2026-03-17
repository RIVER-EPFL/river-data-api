use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS sync_events (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                service_id UUID NOT NULL REFERENCES sync_services(id) ON DELETE CASCADE,
                command_id UUID REFERENCES sync_commands(id) ON DELETE SET NULL,
                event_type TEXT NOT NULL DEFAULT 'scheduled',
                status TEXT NOT NULL DEFAULT 'running',
                readings_synced BIGINT NOT NULL DEFAULT 0,
                status_events_synced BIGINT NOT NULL DEFAULT 0,
                errors JSONB,
                started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                completed_at TIMESTAMPTZ,
                duration_ms BIGINT
            )",
        )
        .await?;

        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_sync_events_service_id ON sync_events(service_id)",
        )
        .await?;

        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_sync_events_started_at ON sync_events(started_at DESC)",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP TABLE IF EXISTS sync_events").await?;
        Ok(())
    }
}
