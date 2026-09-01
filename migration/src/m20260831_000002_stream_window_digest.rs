use sea_orm_migration::prelude::*;

/// The windowed-ingest handshake: the sync client digests each windowed payload's
/// source-asserted content, and the server persists that claim when the pass applies cleanly
/// (no brake, no holds, no rejections). The stream list echoes it back, so the next cycle can
/// skip re-sending content identical to what was last cleanly applied. Opaque to the server;
/// never computed here. NULL means no clean windowed pass has claimed one, which sends in full.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE data_streams
                 ADD COLUMN IF NOT EXISTS last_window_digest TEXT",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE data_streams DROP COLUMN IF EXISTS last_window_digest")
            .await?;
        Ok(())
    }
}
