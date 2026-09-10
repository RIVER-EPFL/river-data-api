use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait as _, Statement};
use sha2::{Digest, Sha256};

/// Version a standalone derived definition's formula, so a stored value can name the text that
/// produced it (Q89).
///
/// `derived_parameter_definitions.formula` is a mutable column with no history, so editing it
/// rewrites the story of every value the old text produced. An edit is a new calculation rather
/// than a correction of the old one, which is the rule `tool_script_versions` already runs for R
/// scripts and for the formula calculations attached to one; this is the same mechanism for the
/// standalone definitions the per-reading engine serves.
///
/// The other half of Q89's decision, `UNIQUE (output_parameter_id)`, is already in the schema as
/// `idx_derived_definitions_output_parameter` (`m20260910_000007_site_parameter_entry_mode`), so
/// the resolver already has one answer and nothing is added for it here.
///
/// Version 1 is minted from each definition's current text, and the readings that predate this
/// migration are left pointing at nothing: what they were computed with is not recoverable, so
/// claiming today's formula made them would be a confident wrong answer (M134). NULL reads as
/// "predates versioning", the way `readings.ingested_at` NULL does.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = r"
    CREATE TABLE IF NOT EXISTS public.derived_parameter_definition_versions (
        id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
        definition_id  uuid NOT NULL REFERENCES public.derived_parameter_definitions(id) ON DELETE CASCADE,
        version_no     integer NOT NULL,
        formula        text NOT NULL,
        content_hash   text NOT NULL,
        created_by     text,
        created_at     timestamptz NOT NULL DEFAULT now(),
        UNIQUE (definition_id, version_no)
    );

    CREATE INDEX IF NOT EXISTS idx_derived_definition_versions_definition
        ON public.derived_parameter_definition_versions (definition_id, version_no DESC);

    ALTER TABLE public.readings
        ADD COLUMN IF NOT EXISTS derived_version_id uuid;

    CREATE INDEX IF NOT EXISTS idx_readings_derived_version
        ON public.readings (derived_version_id) WHERE derived_version_id IS NOT NULL;
";

pub const DOWN: &str = "
    ALTER TABLE public.readings DROP COLUMN IF EXISTS derived_version_id;
    DROP TABLE IF EXISTS public.derived_parameter_definition_versions;
";

/// A formula's content hash: sha256 over the text itself, so the migration and the runtime
/// minting path compute the same string from the same formula.
#[must_use]
pub fn formula_hash(formula: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(formula.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(UP).await?;

        // Version 1 per definition, hashed here rather than in SQL so the stored hash is the one
        // the runtime helper computes for the same text.
        let rows = db
            .query_all_raw(Statement::from_string(
                sea_orm_migration::sea_orm::DatabaseBackend::Postgres,
                "SELECT d.id::text AS id, d.formula
                   FROM public.derived_parameter_definitions d
                  WHERE NOT EXISTS (
                     SELECT 1 FROM public.derived_parameter_definition_versions v
                      WHERE v.definition_id = d.id
                  )"
                .to_string(),
            ))
            .await?;
        for row in &rows {
            let id: String = row.try_get("", "id")?;
            let formula: String = row.try_get("", "formula")?;
            db.execute_raw(Statement::from_sql_and_values(
                sea_orm_migration::sea_orm::DatabaseBackend::Postgres,
                "INSERT INTO public.derived_parameter_definition_versions
                     (definition_id, version_no, formula, content_hash, created_by)
                 VALUES ($1::uuid, 1, $2, $3, 'migration')",
                [
                    id.into(),
                    formula.clone().into(),
                    formula_hash(&formula).into(),
                ],
            ))
            .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(DOWN).await?;
        Ok(())
    }
}
