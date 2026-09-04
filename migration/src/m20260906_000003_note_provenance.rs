use sea_orm_migration::prelude::*;

/// Provenance columns on `notes`, so a source's field notes can be registered idempotently.
///
/// The same shape `annotations` and `sensors` already carry: the pair is the upsert key, under a
/// partial unique index so hand-entered notes coexist with NULLs under it.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
    ALTER TABLE notes ADD COLUMN IF NOT EXISTS source_system TEXT;
    ALTER TABLE notes ADD COLUMN IF NOT EXISTS source_key TEXT;
    CREATE UNIQUE INDEX IF NOT EXISTS notes_provenance_uniq
        ON notes (source_system, source_key)
        WHERE source_system IS NOT NULL AND source_key IS NOT NULL;
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(UP).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP INDEX IF EXISTS notes_provenance_uniq;
                 ALTER TABLE notes DROP COLUMN IF EXISTS source_key;
                 ALTER TABLE notes DROP COLUMN IF EXISTS source_system;",
            )
            .await?;
        Ok(())
    }
}
