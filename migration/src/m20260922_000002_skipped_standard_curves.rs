use sea_orm_migration::prelude::*;

/// A held standard curve the review chose to leave behind.
///
/// A plan refuses to apply while a curve its source holds is attached to no instrument, and not
/// every portal curve is one the lab wants. The stamp records that choice, so the refusal counts
/// what nobody has ruled on, the curve is never stored, and the readings naming it are dropped at
/// the source instead of arriving uncorrected (Q220).
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.standard_curve_proposals
                     ADD COLUMN IF NOT EXISTS skipped_at timestamptz,
                     ADD COLUMN IF NOT EXISTS skipped_by text",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.standard_curve_proposals
                     DROP COLUMN IF EXISTS skipped_at,
                     DROP COLUMN IF EXISTS skipped_by",
            )
            .await?;
        Ok(())
    }
}
