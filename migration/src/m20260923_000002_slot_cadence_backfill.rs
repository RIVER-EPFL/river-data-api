use sea_orm_migration::prelude::*;

/// Declare `low` the slots `m20260922_000004` left at its `high` default that only grabs feed: a
/// slot whose paired streams are all `spot`, the rule a slot created now takes from its stream, and
/// an unpaired slot holding spot readings and nothing else. A continuous, mixed or empty slot keeps
/// `high`.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
UPDATE public.site_parameters sp
   SET cadence = 'low'
 WHERE sp.cadence = 'high'
   AND (
       (EXISTS (SELECT 1 FROM public.data_streams s WHERE s.site_parameter_id = sp.id)
        AND NOT EXISTS (
            SELECT 1 FROM public.data_streams s
             WHERE s.site_parameter_id = sp.id
               AND s.measurement_type IS DISTINCT FROM 'spot'))
    OR (NOT EXISTS (SELECT 1 FROM public.data_streams s WHERE s.site_parameter_id = sp.id)
        AND EXISTS (
            SELECT 1 FROM public.readings r
             WHERE r.site_id = sp.site_id AND r.parameter_id = sp.parameter_id
               AND r.measurement_type = 'spot')
        AND NOT EXISTS (
            SELECT 1 FROM public.readings r
             WHERE r.site_id = sp.site_id AND r.parameter_id = sp.parameter_id
               AND r.measurement_type IS DISTINCT FROM 'spot'))
   );
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(UP).await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
