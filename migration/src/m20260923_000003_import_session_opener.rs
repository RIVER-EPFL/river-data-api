use sea_orm_migration::prelude::*;

/// Record who opened each chunked upload, so a chunk or an import naming the session is refused to
/// anyone else. A session staged before this has no opener and is read as expired by every caller,
/// which its retention would make it within the hour anyway.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
ALTER TABLE public.csv_import_chunks ADD COLUMN IF NOT EXISTS opened_by text NOT NULL DEFAULT '';
ALTER TABLE public.csv_import_chunks ALTER COLUMN opened_by DROP DEFAULT;
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
                "ALTER TABLE public.csv_import_chunks DROP COLUMN IF EXISTS opened_by",
            )
            .await?;
        Ok(())
    }
}
