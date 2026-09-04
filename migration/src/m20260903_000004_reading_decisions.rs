use sea_orm_migration::prelude::*;

/// Curation is an append-only record and the reading's columns are its projection.
///
/// Every flag, withdrawal, curve choice, pin, value correction and verification is a row in
/// `reading_decisions`; an `AFTER INSERT` trigger writes the one column the kind projects to, in
/// the writer's transaction, so the samples trigger, the aggregates, the public arm, the alarm
/// sweep and every serving query keep reading exactly the columns they read today. A rollback is
/// a decision whose `new.columns` names the state it restores.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            "ALTER TABLE readings ADD COLUMN IF NOT EXISTS unverified BOOLEAN NOT NULL DEFAULT false",
        )
        .await?;

        db.execute_unprepared(
            r"
            CREATE TABLE IF NOT EXISTS reading_decisions (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                stream_id UUID NOT NULL,
                time TIMESTAMPTZ NOT NULL,
                replicate_index SMALLINT,
                kind TEXT NOT NULL CHECK (kind IN (
                    'flag', 'unflag', 'withdraw', 'reassert', 'curve', 'calibration_pin',
                    'instrument_pin', 'slot_move', 'value_correction', 'unverified_entry',
                    'verify', 'reject', 'chain', 'detach', 'return', 'rollback')),
                old JSONB NOT NULL DEFAULT '{}'::jsonb,
                new JSONB NOT NULL DEFAULT '{}'::jsonb,
                actor TEXT NOT NULL,
                at TIMESTAMPTZ NOT NULL DEFAULT now(),
                reason TEXT,
                origin TEXT NOT NULL CHECK (origin IN (
                    'manual', 'sync', 'csv', 'audit', 'chain', 'rollback', 'migration', 'system')),
                supersedes UUID REFERENCES reading_decisions(id),
                rolled_back_by UUID REFERENCES reading_decisions(id),
                set_id UUID
            );
            CREATE INDEX IF NOT EXISTS idx_reading_decisions_key
                ON reading_decisions (stream_id, time, replicate_index, at DESC);
            CREATE INDEX IF NOT EXISTS idx_reading_decisions_set
                ON reading_decisions (set_id) WHERE set_id IS NOT NULL;
            ",
        )
        .await?;

        db.execute_unprepared(
            r#"
            CREATE OR REPLACE FUNCTION reading_decisions_project() RETURNS trigger AS $$
            DECLARE
                c JSONB;
            BEGIN
                IF NEW.kind = 'flag' THEN
                    UPDATE readings SET is_flagged = TRUE, flag_reason = NEW.new ->> 'reason'
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind = 'unflag' THEN
                    UPDATE readings SET is_flagged = FALSE, flag_reason = NULL
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind IN ('withdraw', 'reject') THEN
                    UPDATE readings
                       SET withdrawn_at = COALESCE((NEW.new ->> 'withdrawn_at')::timestamptz, NEW.at),
                           withdrawn_reason = NEW.new ->> 'reason',
                           unverified = CASE WHEN NEW.kind = 'reject' THEN FALSE ELSE unverified END
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind = 'reassert' THEN
                    UPDATE readings SET withdrawn_at = NULL, withdrawn_reason = NULL
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind = 'curve' THEN
                    UPDATE readings SET standard_curve_id = (NEW.new ->> 'standard_curve_id')::uuid
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind = 'calibration_pin' THEN
                    UPDATE readings SET calibration_id = (NEW.new ->> 'calibration_id')::uuid
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind = 'instrument_pin' THEN
                    UPDATE readings SET sensor_id = (NEW.new ->> 'sensor_id')::uuid
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind = 'value_correction' THEN
                    UPDATE readings
                       SET raw_value = (NEW.new ->> 'raw_value')::double precision,
                           calibrated_value = NULL
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND replicate_index = NEW.replicate_index;
                ELSIF NEW.kind = 'unverified_entry' THEN
                    UPDATE readings SET unverified = TRUE
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind = 'verify' THEN
                    UPDATE readings SET unverified = FALSE
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                ELSIF NEW.kind = 'rollback' THEN
                    c := NEW.new -> 'columns';
                    UPDATE readings SET
                        is_flagged = CASE WHEN c ? 'is_flagged' THEN (c ->> 'is_flagged')::boolean ELSE is_flagged END,
                        flag_reason = CASE WHEN c ? 'flag_reason' THEN c ->> 'flag_reason' ELSE flag_reason END,
                        withdrawn_at = CASE WHEN c ? 'withdrawn_at' THEN (c ->> 'withdrawn_at')::timestamptz ELSE withdrawn_at END,
                        withdrawn_reason = CASE WHEN c ? 'withdrawn_reason' THEN c ->> 'withdrawn_reason' ELSE withdrawn_reason END,
                        standard_curve_id = CASE WHEN c ? 'standard_curve_id' THEN (c ->> 'standard_curve_id')::uuid ELSE standard_curve_id END,
                        sensor_id = CASE WHEN c ? 'sensor_id' THEN (c ->> 'sensor_id')::uuid ELSE sensor_id END,
                        calibration_id = CASE WHEN c ? 'calibration_id' THEN (c ->> 'calibration_id')::uuid ELSE calibration_id END,
                        raw_value = CASE WHEN c ? 'raw_value' THEN (c ->> 'raw_value')::double precision ELSE raw_value END,
                        calibrated_value = CASE WHEN c ? 'raw_value' THEN NULL ELSE calibrated_value END,
                        unverified = CASE WHEN c ? 'unverified' THEN (c ->> 'unverified')::boolean ELSE unverified END
                     WHERE stream_id = NEW.stream_id AND time = NEW.time
                       AND (NEW.replicate_index IS NULL OR replicate_index = NEW.replicate_index);
                END IF;
                RETURN NEW;
            END;
            $$ LANGUAGE plpgsql;

            DROP TRIGGER IF EXISTS trg_reading_decisions_project ON reading_decisions;
            CREATE TRIGGER trg_reading_decisions_project
                AFTER INSERT ON reading_decisions
                FOR EACH ROW EXECUTE FUNCTION reading_decisions_project();
            "#,
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "DROP TRIGGER IF EXISTS trg_reading_decisions_project ON reading_decisions;
             DROP FUNCTION IF EXISTS reading_decisions_project();
             DROP TABLE IF EXISTS reading_decisions;
             ALTER TABLE readings DROP COLUMN IF EXISTS unverified",
        )
        .await?;
        Ok(())
    }
}
