use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // The value 2e-6 is a dimensionless fraction, copied verbatim from the portal; the '%'
        // label came with it and describes neither. The mismatch is 10^6-sensitive in the
        // dissolved-CH4 formula, so the label must not invite a percentage entry.
        manager
            .get_connection()
            .execute_unprepared(
                "UPDATE constants SET units = NULL, \
                        description = 'Fraction of CH4 in standard air (dimensionless)' \
                 WHERE name = 'ch4_in_sa'",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "UPDATE constants SET units = '%', description = 'Fraction of CH4 in SA' \
                 WHERE name = 'ch4_in_sa'",
            )
            .await?;
        Ok(())
    }
}
