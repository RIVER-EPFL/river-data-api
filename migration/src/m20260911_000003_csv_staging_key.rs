use sea_orm_migration::prelude::*;

/// Declare the key `csv_import_staging` already has.
///
/// The importer numbers `seq` from zero within each `import_token` and the worker reads the set
/// back in that order, so the pair identifies a staged row. Stating it lets the table carry an
/// entity, which is what takes the insert off a hand-numbered placeholder list, and lets one bad
/// row be addressed instead of the whole token.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Staging rows live only for the length of an import, so a run in flight when this lands
        // is dropped rather than migrated: a duplicate pair would refuse the constraint, and the
        // import it belongs to is re-uploaded.
        manager
            .get_connection()
            .execute_unprepared("DELETE FROM public.csv_import_staging")
            .await?;
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.csv_import_staging
                     ADD CONSTRAINT csv_import_staging_pkey
                     PRIMARY KEY (import_token, seq)",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.csv_import_staging
                     DROP CONSTRAINT IF EXISTS csv_import_staging_pkey",
            )
            .await?;
        Ok(())
    }
}
