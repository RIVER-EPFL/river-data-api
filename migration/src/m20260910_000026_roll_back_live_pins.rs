use sea_orm_migration::prelude::*;

/// Roll back the attribution pins still live when their surfaces were retired (Q117, B202).
///
/// Nothing deletes a decision, so the `instrument_pin` and `calibration_pin` rows recorded before
/// the routes went keep their hold: every reprocess, resolver and backfill steps over the readings
/// they name, and the route that could have cleared one is gone. Each live pin is inverted the way
/// `decisions::rollback` did it, by appending a `rollback` carrying the state the pin recorded, so
/// the projection trigger restores the column and the ledger keeps both halves. The slots those
/// readings belong to are then queued for the reprocess that re-derives them from the deployment
/// and calibration history.
#[derive(DeriveMigrationName)]
pub struct Migration;

/// Why the rollbacks say they happened, and how the second statement finds the rows the first
/// wrote.
const REASON: &str = "the attribution pin surfaces were retired (Q117)";

#[must_use]
pub fn roll_back_live_pins() -> String {
    format!(
        r"
        WITH live AS (
            SELECT DISTINCT ON (d.id)
                   d.id, d.stream_id, d.time, d.replicate_index, d.kind, d.old,
                   CASE WHEN d.kind = 'instrument_pin'
                        THEN jsonb_build_object('sensor_id', to_jsonb(r.sensor_id))
                        ELSE jsonb_build_object('calibration_id', to_jsonb(r.calibration_id))
                   END AS now_state
              FROM reading_decisions d
              JOIN readings r
                ON r.stream_id = d.stream_id AND r.time = d.time
               AND (d.replicate_index IS NULL OR r.replicate_index = d.replicate_index)
             WHERE d.kind IN ('instrument_pin', 'calibration_pin')
               AND d.rolled_back_by IS NULL
             ORDER BY d.id, r.replicate_index
        ), inverted AS (
            INSERT INTO reading_decisions
                (stream_id, time, replicate_index, kind, old, new, actor, reason, origin)
            SELECT l.stream_id, l.time, l.replicate_index, 'rollback', l.now_state,
                   jsonb_build_object('columns', l.old, 'of', l.id),
                   'system', '{REASON}', 'rollback'
              FROM live l
            RETURNING id, ((new ->> 'of')::uuid) AS pin_id
        )
        UPDATE reading_decisions d
           SET rolled_back_by = inverted.id
          FROM inverted
         WHERE d.id = inverted.pin_id;

        UPDATE reading_decision_sets s
           SET rolled_back_at = now(), rolled_back_by = 'system'
         WHERE s.rolled_back_at IS NULL
           AND s.kind IN ('instrument_pin', 'calibration_pin')
           AND NOT EXISTS (
                 SELECT 1 FROM reading_decisions d
                  WHERE d.set_id = s.id AND d.rolled_back_by IS NULL
                    AND d.kind IN ('instrument_pin', 'calibration_pin'));

        INSERT INTO reprocessing_jobs
            (id, trigger_type, status, category, params, dedupe_key, next_attempt_at)
        SELECT gen_random_uuid(), 'attribution_pin', 'queued', 'operator',
               jsonb_build_object('site_id', r.site_id, 'parameter_id', r.parameter_id),
               'attribution_pin_retire:' || r.site_id || ':' || r.parameter_id, now()
          FROM reading_decisions d
          JOIN readings r
            ON r.stream_id = d.stream_id AND r.time = d.time
           AND (d.replicate_index IS NULL OR r.replicate_index = d.replicate_index)
         WHERE d.kind = 'rollback' AND d.reason = '{REASON}'
           AND r.site_id IS NOT NULL AND r.parameter_id IS NOT NULL
         GROUP BY r.site_id, r.parameter_id
            ON CONFLICT (dedupe_key) WHERE dedupe_key IS NOT NULL DO NOTHING;
    "
    )
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(&roll_back_live_pins())
            .await?;
        Ok(())
    }

    /// The pins are restored by taking their rollbacks back off, which is the same shape the
    /// forward direction wrote: the queued reprocess rows are left, since a slot re-derived from
    /// its own history is not damage.
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(&format!(
                r"
                UPDATE reading_decisions d SET rolled_back_by = NULL
                  FROM reading_decisions rb
                 WHERE rb.kind = 'rollback' AND rb.reason = '{REASON}'
                   AND d.id = ((rb.new ->> 'of')::uuid);
                DELETE FROM reading_decisions WHERE kind = 'rollback' AND reason = '{REASON}';
                UPDATE reading_decision_sets SET rolled_back_at = NULL, rolled_back_by = NULL
                 WHERE rolled_back_by = 'system'
                   AND kind IN ('instrument_pin', 'calibration_pin');
            "
            ))
            .await?;
        Ok(())
    }
}
