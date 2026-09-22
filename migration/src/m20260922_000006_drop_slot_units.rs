use sea_orm_migration::prelude::*;

/// Units are the catalog's. A slot carried a per-site override that no writer set and the public
/// API never served, plus three units columns with no reader at all.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
ALTER TABLE public.site_parameters
    DROP COLUMN display_units,
    DROP COLUMN units_name,
    DROP COLUMN units_min,
    DROP COLUMN units_max;
";

const DOWN: &str = r"
ALTER TABLE public.site_parameters
    ADD COLUMN display_units character varying(32),
    ADD COLUMN units_name character varying(64),
    ADD COLUMN units_min double precision,
    ADD COLUMN units_max double precision;
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
