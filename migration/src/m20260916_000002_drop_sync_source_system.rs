use sea_orm_migration::prelude::*;

/// A sync service no longer declares the source system it registers under (Q218): the name comes
/// from each registration, as it did before the declaration existed.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
ALTER TABLE public.sync_service_credentials DROP COLUMN source_system;
ALTER TABLE public.sync_services DROP COLUMN source_system;
";

const DOWN: &str = r"
ALTER TABLE public.sync_service_credentials ADD COLUMN source_system character varying(64);
ALTER TABLE public.sync_services ADD COLUMN source_system character varying(64);
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(UP).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(DOWN).await?;
        Ok(())
    }
}
