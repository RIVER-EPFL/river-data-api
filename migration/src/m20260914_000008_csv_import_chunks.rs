use sea_orm_migration::prelude::*;

/// Where a chunked CSV upload accumulates.
///
/// The appends used to grow one `Arc<String>` in a 500 MB in-process cache, which the portal's own
/// 474 MB file fills on its own and which no second replica can read. A row per chunk keyed by the
/// session puts the file where every replica sees it and where its size is the database's problem
/// rather than a cache's eviction.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS public.csv_import_chunks (
                     session_id uuid NOT NULL,
                     seq integer NOT NULL,
                     chunk text NOT NULL,
                     created_at timestamp with time zone DEFAULT now() NOT NULL,
                     CONSTRAINT csv_import_chunks_pkey PRIMARY KEY (session_id, seq)
                 );

                 CREATE INDEX IF NOT EXISTS idx_csv_import_chunks_created_at
                     ON public.csv_import_chunks USING btree (created_at);",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS public.csv_import_chunks")
            .await?;
        Ok(())
    }
}
