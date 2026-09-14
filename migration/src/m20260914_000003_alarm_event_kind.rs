use sea_orm_migration::prelude::*;

/// Say what raised an alarm episode, and which instrument it is about.
///
/// A miniDOT reading 32 mg/L of oxygen and a river genuinely above its warning bound produced the
/// same row: one severity, one parameter, nothing saying whether the instrument or the water is
/// the unusual one. `kind` separates the two and the open-unique index carries it, so a range
/// episode and a threshold episode stand open on the same slot at once.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.alarm_events
                     ADD COLUMN IF NOT EXISTS kind character varying(32)
                         DEFAULT 'threshold'::character varying NOT NULL,
                     ADD COLUMN IF NOT EXISTS sensor_id uuid REFERENCES public.sensors(id)
                         ON DELETE SET NULL;

                 ALTER TABLE public.alarm_events
                     DROP CONSTRAINT IF EXISTS alarm_events_kind_check;
                 ALTER TABLE public.alarm_events
                     ADD CONSTRAINT alarm_events_kind_check
                     CHECK (kind IN ('threshold', 'instrument_range'));

                 DROP INDEX IF EXISTS uq_alarm_events_open;
                 CREATE UNIQUE INDEX uq_alarm_events_open ON public.alarm_events
                     USING btree (site_id, parameter_id, measurement_type, kind)
                     WHERE (resolved_at IS NULL);",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DELETE FROM public.alarm_events WHERE kind = 'instrument_range';

                 DROP INDEX IF EXISTS uq_alarm_events_open;
                 CREATE UNIQUE INDEX uq_alarm_events_open ON public.alarm_events
                     USING btree (site_id, parameter_id, measurement_type)
                     WHERE (resolved_at IS NULL);

                 ALTER TABLE public.alarm_events
                     DROP CONSTRAINT IF EXISTS alarm_events_kind_check,
                     DROP COLUMN IF EXISTS kind,
                     DROP COLUMN IF EXISTS sensor_id;",
            )
            .await?;
        Ok(())
    }
}
