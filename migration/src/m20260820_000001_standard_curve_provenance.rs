use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Where a curve came from, for curves the sync services replicate out of a portal's own
        // standard_curves table (e.g. "cnet" / "standard_curves:17"). Hand-entered curves keep
        // NULLs. The pair is what makes the sync upsert idempotent: re-registering the same portal
        // curve resolves to the same row instead of minting a duplicate every cycle.
        db.execute_unprepared(
            "ALTER TABLE standard_curves
             ADD COLUMN IF NOT EXISTS source_system TEXT,
             ADD COLUMN IF NOT EXISTS source_key TEXT",
        )
        .await?;

        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS standard_curves_provenance_uniq
             ON standard_curves (source_system, source_key)
             WHERE source_system IS NOT NULL AND source_key IS NOT NULL",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS standard_curves_provenance_uniq")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE standard_curves
             DROP COLUMN IF EXISTS source_system,
             DROP COLUMN IF EXISTS source_key",
        )
        .await?;
        Ok(())
    }
}
