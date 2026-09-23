use sea_orm_migration::prelude::*;

/// Declare `low` an unpaired calculation-output slot left `high` over inputs that are no longer
/// all `high` at its site: `applied_cadence` over `cadence_deciding`, the rule Apply calculation
/// uses, read against the inputs `m20260923_000002` lowered. The inputs that decide are the ones
/// the active version reads at the instant, its event inputs not held from the last visit and the
/// parameters its replicate params name. A paired slot keeps its stream's declaration, and no slot
/// is raised: the chain mints output slots `low` whatever their inputs.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
WITH output_slots AS (
    SELECT sp.id AS slot_id, sp.site_id, v.manifest
      FROM public.site_parameters sp
      JOIN public.parameters op ON op.id = sp.parameter_id
      JOIN public.tool_scripts ts ON ts.active_version_id IS NOT NULL
      JOIN public.tool_script_versions v ON v.id = ts.active_version_id
     WHERE sp.cadence = 'high'
       AND NOT EXISTS (SELECT 1 FROM public.data_streams s WHERE s.site_parameter_id = sp.id)
       AND EXISTS (
           SELECT 1 FROM jsonb_array_elements(COALESCE(v.manifest->'outputs', '[]'::jsonb)) o
            WHERE o->>'parameter_id' = op.id::text
               OR LOWER(o->>'suggested_parameter_code') = LOWER(op.code))
),
deciding AS (
    SELECT os.slot_id, isp.cadence
      FROM output_slots os
      JOIN public.site_parameters isp ON isp.site_id = os.site_id
      JOIN public.parameters ip ON ip.id = isp.parameter_id
     WHERE LOWER(ip.code) IN (
           SELECT LOWER(e->>'parameter_code')
             FROM jsonb_array_elements(COALESCE(os.manifest->'event_inputs', '[]'::jsonb)) e
            WHERE COALESCE(e->>'alignment', '') <> 'hold'
           UNION
           SELECT LOWER(p->>'parameter_code')
             FROM jsonb_array_elements(COALESCE(os.manifest->'params', '[]'::jsonb)) p
            WHERE p->>'kind' = 'replicates' AND p->>'parameter_code' IS NOT NULL)
)
UPDATE public.site_parameters sp
   SET cadence = 'low'
  FROM output_slots os
 WHERE sp.id = os.slot_id
   AND NOT (
       EXISTS (SELECT 1 FROM deciding d WHERE d.slot_id = os.slot_id)
       AND NOT EXISTS (
           SELECT 1 FROM deciding d WHERE d.slot_id = os.slot_id AND d.cadence <> 'high'));
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
