use sea_orm_migration::prelude::*;

/// Every reading awaiting a manager's ruling has its review-queue row: one pending
/// `unverified_entry` hold per slot instant carrying an unverified, unwithdrawn reading and no open
/// hold of that kind, naming whoever the ledger says entered it. A superseded hold is not
/// reopenable, so the row is a new one.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r#"
INSERT INTO public.replicate_audit_holds
    (site_id, parameter_id, group_time, kind, expected, computed, delta, status)
SELECT DISTINCT ON (r.site_id, r.parameter_id, r.time)
       r.site_id, r.parameter_id, r.time, 'unverified_entry',
       '{"state": "verified"}'::jsonb,
       jsonb_build_object('state', 'unverified', 'entered_by', COALESCE(d.actor, 'system')),
       '{}'::jsonb, 'pending'
  FROM public.readings r
  LEFT JOIN LATERAL (
       SELECT rd.actor FROM public.reading_decisions rd
        WHERE rd.stream_id = r.stream_id AND rd.time = r.time
          AND (rd.replicate_index IS NULL OR rd.replicate_index = r.replicate_index)
          AND rd.kind = 'unverified_entry'
        ORDER BY rd.at DESC
        LIMIT 1) d ON true
 WHERE r.unverified IS TRUE
   AND r.withdrawn_at IS NULL
   AND r.site_id IS NOT NULL
   AND r.parameter_id IS NOT NULL
   AND NOT EXISTS (
       SELECT 1 FROM public.replicate_audit_holds h
        WHERE h.kind = 'unverified_entry' AND h.stream_id IS NULL
          AND h.site_id = r.site_id AND h.parameter_id = r.parameter_id
          AND h.group_time = r.time AND h.status IN ('pending', 'deferred'))
 ORDER BY r.site_id, r.parameter_id, r.time, r.replicate_index
ON CONFLICT DO NOTHING;
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
