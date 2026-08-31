use sea_orm_migration::prelude::*;

/// Where a source-authored annotation came from, for annotations the sync services register out
/// of a portal (e.g. a note that a value was corrected at source with a named standard curve).
/// Hand-entered annotations keep NULLs. The pair is what makes the sync upsert idempotent: a
/// full-content pass re-asserting the same key updates the row instead of minting a duplicate
/// every cycle.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "ALTER TABLE annotations
             ADD COLUMN IF NOT EXISTS source_system TEXT,
             ADD COLUMN IF NOT EXISTS source_key TEXT",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS annotations_provenance_uniq
             ON annotations (source_system, source_key)
             WHERE source_system IS NOT NULL AND source_key IS NOT NULL",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS annotations_provenance_uniq")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE annotations
             DROP COLUMN IF EXISTS source_system,
             DROP COLUMN IF EXISTS source_key",
        )
        .await?;
        Ok(())
    }
}
