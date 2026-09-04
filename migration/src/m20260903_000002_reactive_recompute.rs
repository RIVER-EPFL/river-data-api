use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // A calculation can be authored, validated and activated without firing at visits until
        // it is switched on. Existing tools were live before the switch existed and stay live.
        db.execute_unprepared(
            "ALTER TABLE tool_scripts ADD COLUMN IF NOT EXISTS enabled BOOLEAN NOT NULL DEFAULT true",
        )
        .await?;

        // The visits grid reads each event's latest recompute job by the event id the job's
        // params carry; a per-event lookup over the whole job table needs this to stay cheap.
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_reprocessing_jobs_event_recompute
             ON reprocessing_jobs ((params ->> 'collection_event_id'), created_at DESC)
             WHERE trigger_type = 'event_recompute'",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_reprocessing_jobs_event_recompute")
            .await?;
        db.execute_unprepared("ALTER TABLE tool_scripts DROP COLUMN IF EXISTS enabled")
            .await?;
        Ok(())
    }
}
