use sea_orm_migration::prelude::*;

/// One calculation entity over two engines.
///
/// `tool_scripts` is the calculation: `engine` says whether its versions carry an R script or a
/// set of formulas, and `parameter_group_id` binds it to the group whose members it reads and
/// writes (one calculation per group). A formula calculation's formulas are
/// `derived_parameter_definitions` rows carrying `tool_script_id` and an `ordinal`; they are
/// versioned the way script versions are, as a `tool_script_versions` row minted from the formula
/// set, so a run pins the shape it was made under and the audit recomputes against it.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = "
    ALTER TABLE public.tool_scripts ADD COLUMN IF NOT EXISTS engine text NOT NULL DEFAULT 'script';
    ALTER TABLE public.tool_scripts DROP CONSTRAINT IF EXISTS tool_scripts_engine_check;
    ALTER TABLE public.tool_scripts ADD CONSTRAINT tool_scripts_engine_check
        CHECK (engine IN ('script', 'formula'));

    ALTER TABLE public.tool_scripts ADD COLUMN IF NOT EXISTS parameter_group_id uuid
        REFERENCES public.parameter_groups(id);
    CREATE UNIQUE INDEX IF NOT EXISTS idx_tool_scripts_parameter_group
        ON public.tool_scripts (parameter_group_id) WHERE parameter_group_id IS NOT NULL;

    ALTER TABLE public.derived_parameter_definitions
        ADD COLUMN IF NOT EXISTS tool_script_id uuid
            REFERENCES public.tool_scripts(id) ON DELETE CASCADE;
    ALTER TABLE public.derived_parameter_definitions
        ADD COLUMN IF NOT EXISTS ordinal integer NOT NULL DEFAULT 0;
    CREATE INDEX IF NOT EXISTS idx_derived_definitions_calculation
        ON public.derived_parameter_definitions (tool_script_id, ordinal);
";

const DOWN: &str = "
    DROP INDEX IF EXISTS public.idx_derived_definitions_calculation;
    ALTER TABLE public.derived_parameter_definitions DROP COLUMN IF EXISTS ordinal;
    ALTER TABLE public.derived_parameter_definitions DROP COLUMN IF EXISTS tool_script_id;
    DROP INDEX IF EXISTS public.idx_tool_scripts_parameter_group;
    ALTER TABLE public.tool_scripts DROP COLUMN IF EXISTS parameter_group_id;
    ALTER TABLE public.tool_scripts DROP CONSTRAINT IF EXISTS tool_scripts_engine_check;
    ALTER TABLE public.tool_scripts DROP COLUMN IF EXISTS engine;
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
