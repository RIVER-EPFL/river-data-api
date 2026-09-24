use sea_orm_migration::prelude::*;

/// A grab row that repeats a live replicate another stream holds at the same site, parameter,
/// instant, index and value is a second copy of that measurement: it is withdrawn on the ledger,
/// and the sample recomputes without it.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r#"
INSERT INTO public.reading_decisions
    (stream_id, "time", replicate_index, kind, old, new, actor, reason, origin)
SELECT g.stream_id, g.time, g.replicate_index, 'withdraw',
       '{"withdrawn_at": null, "withdrawn_reason": null}'::jsonb,
       '{"reason": "a copy of a replicate another stream holds (B652)"}'::jsonb,
       'system', 'a copy of a replicate another stream holds (B652)', 'system'
  FROM public.readings g
  JOIN public.data_streams s ON s.id = g.stream_id AND s.source_system = 'grab_sample'
 WHERE g.measurement_type = 'spot'
   AND g.withdrawn_at IS NULL
   AND EXISTS (
       SELECT 1 FROM public.readings o
        WHERE o.site_id = g.site_id AND o.parameter_id = g.parameter_id
          AND o.time = g.time AND o.measurement_type = 'spot'
          AND o.stream_id <> g.stream_id
          AND o.replicate_index = g.replicate_index
          AND o.raw_value = g.raw_value
          AND o.withdrawn_at IS NULL);
"#;

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
