use sea_orm_migration::prelude::*;

/// Which sync services get the weekly full re-assert becomes a setting on the service.
///
/// The list was `SYNC_FULL_REASSERT_SERVICE_TYPES`, an API environment variable naming service
/// *types*, so turning one instance off meant an API rollout and took every instance of that type
/// with it. The flag is backfilled true for `rshiny`, the one type the variable's default named, so
/// a database carried forward keeps re-asserting exactly what it was re-asserting.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.sync_services \
                     ADD COLUMN IF NOT EXISTS full_reassert_enabled BOOLEAN NOT NULL DEFAULT false; \
                 UPDATE public.sync_services SET full_reassert_enabled = true \
                  WHERE service_type = 'rshiny';",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.sync_services DROP COLUMN IF EXISTS full_reassert_enabled;",
            )
            .await?;
        Ok(())
    }
}
