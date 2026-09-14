use sea_orm_migration::prelude::*;

/// What the last fetch of a MeteoSwiss URL returned, so the next one can be conditional.
///
/// MeteoSwiss ask for `If-None-Match` rather than a polling limit, and nothing held an ETag, so
/// every tick re-downloaded bytes it already had.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS public.meteoswiss_fetch_state (
                     url text NOT NULL PRIMARY KEY,
                     etag text,
                     fetched_at timestamp with time zone DEFAULT now() NOT NULL
                 )",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS public.meteoswiss_fetch_state")
            .await?;
        Ok(())
    }
}
