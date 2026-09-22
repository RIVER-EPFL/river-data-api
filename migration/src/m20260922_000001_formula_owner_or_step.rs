use sea_orm_migration::prelude::*;

/// A formula belongs to a calculation or is a shared step; the standalone derived definition is
/// gone (Q231).
///
/// A derived parameter is an output of a versioned formula calculation or of an R script, so the
/// per-formula version ledger has no writer left and no second answer to give about which text
/// made a value.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
ALTER TABLE public.calculation_formulas
    ADD CONSTRAINT calculation_formulas_owner_or_step
    CHECK (tool_script_id IS NOT NULL OR intermediate);
DROP TABLE public.derived_parameter_definition_versions;
";

const DOWN: &str = r"
CREATE TABLE public.derived_parameter_definition_versions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    definition_id uuid NOT NULL,
    version_no integer NOT NULL,
    formula text NOT NULL,
    content_hash text NOT NULL,
    created_by text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);
ALTER TABLE ONLY public.derived_parameter_definition_versions
    ADD CONSTRAINT derived_parameter_definition_versions_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.derived_parameter_definition_versions
    ADD CONSTRAINT derived_parameter_definition_versi_definition_id_version_no_key
    UNIQUE (definition_id, version_no);
ALTER TABLE ONLY public.derived_parameter_definition_versions
    ADD CONSTRAINT derived_parameter_definition_versions_definition_id_fkey
    FOREIGN KEY (definition_id) REFERENCES public.calculation_formulas(id) ON DELETE CASCADE;
CREATE INDEX idx_derived_definition_versions_definition
    ON public.derived_parameter_definition_versions USING btree (definition_id, version_no DESC);
ALTER TABLE public.calculation_formulas DROP CONSTRAINT calculation_formulas_owner_or_step;
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
