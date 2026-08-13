use sea_orm_migration::prelude::*;

/// Records whether a calibration's `valid_until` was set by an operator or written by the window
/// chain. The row alone cannot carry that distinction, and the chain must only ever shorten an
/// operator-set bound instead of rewriting it. Every existing bound was chain-written, so the
/// default is correct with no backfill.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE sensor_calibrations
                 ADD COLUMN IF NOT EXISTS valid_until_explicit BOOLEAN NOT NULL DEFAULT FALSE",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE sensor_calibrations DROP COLUMN IF EXISTS valid_until_explicit",
            )
            .await?;
        Ok(())
    }
}
