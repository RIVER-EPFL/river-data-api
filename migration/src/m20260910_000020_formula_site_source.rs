use sea_orm_migration::prelude::*;

/// A formula source that names a column of the site's own row rather than a parameter.
///
/// A station's elevation is not something measured at a visit, so it has no `parameters` row and
/// no reading to read. It is a property of the site, which the calculate path already resolves for
/// script tools through the manifest's `site_inputs`; this is the same source expressed where a
/// formula's inputs live. A row names one or the other, never both.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = "
    ALTER TABLE public.derived_parameter_sources
        ALTER COLUMN parameter_id DROP NOT NULL,
        ADD COLUMN IF NOT EXISTS site_property text;

    ALTER TABLE public.derived_parameter_sources
        DROP CONSTRAINT IF EXISTS derived_parameter_sources_one_source;
    ALTER TABLE public.derived_parameter_sources
        ADD CONSTRAINT derived_parameter_sources_one_source
        CHECK (num_nonnulls(parameter_id, site_property) = 1);

    CREATE UNIQUE INDEX IF NOT EXISTS uq_derived_site_property
        ON public.derived_parameter_sources (derived_definition_id, site_property)
        WHERE site_property IS NOT NULL;
";

pub const DOWN: &str = "
    DROP INDEX IF EXISTS public.uq_derived_site_property;
    ALTER TABLE public.derived_parameter_sources
        DROP CONSTRAINT IF EXISTS derived_parameter_sources_one_source;
    DELETE FROM public.derived_parameter_sources WHERE parameter_id IS NULL;
    ALTER TABLE public.derived_parameter_sources
        DROP COLUMN IF EXISTS site_property,
        ALTER COLUMN parameter_id SET NOT NULL;
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
