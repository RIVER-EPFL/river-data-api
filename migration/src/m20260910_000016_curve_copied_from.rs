use sea_orm_migration::prelude::*;

/// A standard curve copied onto another instrument says which curve it came from.
///
/// A curve belongs to exactly one instrument (`standard_curves.sensor_id NOT NULL`), so readings
/// split onto another instrument cannot take theirs with them. Q112's decision is that the curve is
/// copied over rather than moved or shared, and a copy that does not name its original is a second
/// curve nobody can tell from a separately fitted one.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = "
    ALTER TABLE public.standard_curves
        ADD COLUMN IF NOT EXISTS copied_from_id uuid REFERENCES public.standard_curves(id);

    CREATE INDEX IF NOT EXISTS idx_standard_curves_copied_from
        ON public.standard_curves (copied_from_id)
        WHERE copied_from_id IS NOT NULL;
";

const DOWN: &str = "
    DROP INDEX IF EXISTS public.idx_standard_curves_copied_from;
    ALTER TABLE public.standard_curves DROP COLUMN IF EXISTS copied_from_id;
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
