use sea_orm_migration::prelude::*;

/// The source system a sync service speaks for, declared at credential mint.
///
/// `service_type` is the kind of service, not the source: one rshiny image serves CNET, METALP or
/// NOMIS by `PORTAL_TYPE`, and `m20260907_000001_full_reassert_per_service` reaches all three as
/// `service_type = 'rshiny'`. Until now the source system arrived as a string in each register
/// call's body, so nothing tied the provenance a service wrote to the identity it authenticated
/// with. It is declared once here and read off the caller instead.
///
/// NULL where an existing credential never declared one. The one case that needs no operator is
/// `vaisala`, where the kind and the system are the same word.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = r"
    ALTER TABLE public.sync_service_credentials
        ADD COLUMN IF NOT EXISTS source_system character varying(64);
    ALTER TABLE public.sync_services
        ADD COLUMN IF NOT EXISTS source_system character varying(64);
    UPDATE public.sync_service_credentials SET source_system = 'vaisala'
        WHERE source_system IS NULL AND service_type = 'vaisala';
    UPDATE public.sync_services SET source_system = 'vaisala'
        WHERE source_system IS NULL AND service_type = 'vaisala';
";

pub const DOWN: &str = r"
    ALTER TABLE public.sync_services DROP COLUMN IF EXISTS source_system;
    ALTER TABLE public.sync_service_credentials DROP COLUMN IF EXISTS source_system;
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
