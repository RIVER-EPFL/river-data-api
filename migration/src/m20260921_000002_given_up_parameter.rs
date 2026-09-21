use sea_orm_migration::prelude::*;

/// The catalog parameter a formula published and gave up, recorded rather than inferred.
///
/// Publication is enabled and disabled through the step tick: a formula ticked as a step gives up
/// its `output_parameter_id`, and until now nothing said which row it had been. Ticking it back
/// therefore reached the adoption guard, which refuses a code the catalog already holds, so the
/// same parameter could not be recovered. The column names it, so the round trip is by id and
/// survives a rename in between.
///
/// Existing rows are backfilled from the code, which is the evidence the catalogue read before
/// this column existed.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.calculation_formulas
                     ADD COLUMN IF NOT EXISTS given_up_parameter_id uuid
                     REFERENCES public.parameters(id) ON DELETE SET NULL;

                 UPDATE public.calculation_formulas f
                    SET given_up_parameter_id = p.id
                   FROM public.parameters p
                  WHERE f.output_parameter_id IS NULL
                    AND f.given_up_parameter_id IS NULL
                    AND LOWER(p.code) = LOWER(f.code);",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.calculation_formulas
                     DROP COLUMN IF EXISTS given_up_parameter_id",
            )
            .await?;
        Ok(())
    }
}
