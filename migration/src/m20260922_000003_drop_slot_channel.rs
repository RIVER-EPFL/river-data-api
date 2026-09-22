use sea_orm_migration::prelude::*;

/// A slot's channel is written by no path and read by nothing but its own form (D36): the Vaisala
/// channel lives in the stream's metadata, and copying it onto the slot is the pairing's work if
/// it is ever wanted.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
ALTER TABLE public.site_parameters DROP COLUMN channel_id;
";

const DOWN: &str = r"
ALTER TABLE public.site_parameters ADD COLUMN channel_id integer;
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
