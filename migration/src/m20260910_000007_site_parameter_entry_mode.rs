use sea_orm_migration::prelude::*;

/// How a site's slot is filled, said once.
///
/// Q68 settled that the group is a grouping: which calculation produces a parameter is the
/// group's calculation binding, while whether a given site fills its slot by hand or by
/// calculation is the definition of the site parameter. So `derived_definition_id` goes and the
/// per-site declaration stays under the name it now means, `entry_mode`.
///
/// The definition a slot's values come from is the one whose `output_parameter_id` is the slot's
/// parameter, which is what the dropped column duplicated. The partial unique index is what makes
/// that resolution single-valued, and it is the invariant T23 states in SQL: an output is produced
/// by exactly one calculation.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = "
    ALTER TABLE public.site_parameters
        ADD COLUMN IF NOT EXISTS entry_mode text NOT NULL DEFAULT 'manual';

    UPDATE public.site_parameters SET entry_mode = 'tool' WHERE is_derived IS TRUE;

    ALTER TABLE public.site_parameters DROP CONSTRAINT IF EXISTS site_parameters_entry_mode_check;
    ALTER TABLE public.site_parameters ADD CONSTRAINT site_parameters_entry_mode_check
        CHECK (entry_mode IN ('manual', 'tool'));

    CREATE INDEX IF NOT EXISTS idx_site_parameters_entry_mode
        ON public.site_parameters (entry_mode) WHERE entry_mode = 'tool';

    CREATE UNIQUE INDEX IF NOT EXISTS idx_derived_definitions_output_parameter
        ON public.derived_parameter_definitions (output_parameter_id)
        WHERE output_parameter_id IS NOT NULL;

    ALTER TABLE public.site_parameters DROP COLUMN IF EXISTS derived_definition_id;
    ALTER TABLE public.site_parameters DROP COLUMN IF EXISTS is_derived;
";

pub const DOWN: &str = "
    ALTER TABLE public.site_parameters
        ADD COLUMN IF NOT EXISTS is_derived boolean DEFAULT false;
    ALTER TABLE public.site_parameters
        ADD COLUMN IF NOT EXISTS derived_definition_id uuid
            REFERENCES public.derived_parameter_definitions(id);

    UPDATE public.site_parameters SET is_derived = (entry_mode = 'tool');
    UPDATE public.site_parameters sp
       SET derived_definition_id = d.id
      FROM public.derived_parameter_definitions d
     WHERE d.output_parameter_id = sp.parameter_id AND sp.entry_mode = 'tool';

    DROP INDEX IF EXISTS public.idx_derived_definitions_output_parameter;
    DROP INDEX IF EXISTS public.idx_site_parameters_entry_mode;
    ALTER TABLE public.site_parameters DROP CONSTRAINT IF EXISTS site_parameters_entry_mode_check;
    ALTER TABLE public.site_parameters DROP COLUMN IF EXISTS entry_mode;
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
