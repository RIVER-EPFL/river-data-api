use sea_orm_migration::prelude::*;

/// A pairing plan carries the standard curves the review assigns to instruments the plan will
/// create, as `[{curve_id, instrument_source_key}]`, so the move happens in the apply transaction
/// that mints the instrument rather than by hand afterwards.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE pairing_plans \
                 ADD COLUMN IF NOT EXISTS curve_assignments JSONB NOT NULL DEFAULT '[]'::jsonb",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE pairing_plans DROP COLUMN IF EXISTS curve_assignments")
            .await?;
        Ok(())
    }
}
