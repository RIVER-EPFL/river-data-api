use sea_orm_migration::prelude::*;

/// The scheduled sync cadence, set by an operator and carried to the service on enrollment and
/// every heartbeat. NULL leaves the service on its own `SYNC_INTERVAL_SECONDS`, which is what
/// every service ran on before this column existed.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE sync_services
                 ADD COLUMN IF NOT EXISTS sync_interval_secs INTEGER",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE sync_services DROP COLUMN IF EXISTS sync_interval_secs")
            .await?;
        Ok(())
    }
}
