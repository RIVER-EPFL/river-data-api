use sea_orm_migration::prelude::*;

/// A curve that has corrected a reading is retired, not deleted (Q107).
///
/// Retirement takes a curve out of circulation and leaves the row, its provenance and the readings
/// that named it standing. A windowed calibration's readings move onto whatever else covers them,
/// which is a value change and therefore a decision: `curve_retire` is its kind, projected onto
/// `calibration_id` like a pin but a separate family, because a pin is honoured against every later
/// reprocess and a repoint is a derivation a reprocess is meant to redo. A standard curve is named
/// by the reading rather than resolved by window, so retiring one moves no value and writes no
/// decision.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = r#"
    ALTER TABLE public.sensor_calibrations
        ADD COLUMN IF NOT EXISTS retired_at timestamptz,
        ADD COLUMN IF NOT EXISTS retired_by text,
        ADD COLUMN IF NOT EXISTS retired_reason text;

    ALTER TABLE public.standard_curves
        ADD COLUMN IF NOT EXISTS retired_at timestamptz,
        ADD COLUMN IF NOT EXISTS retired_by text,
        ADD COLUMN IF NOT EXISTS retired_reason text;

    ALTER TABLE public.reading_decisions
        DROP CONSTRAINT IF EXISTS reading_decisions_kind_check;
    ALTER TABLE public.reading_decisions
        ADD CONSTRAINT reading_decisions_kind_check CHECK (kind = ANY (ARRAY[
            'flag'::text, 'unflag'::text, 'withdraw'::text, 'reassert'::text, 'curve'::text,
            'calibration_pin'::text, 'instrument_pin'::text, 'slot_move'::text,
            'value_correction'::text, 'unverified_entry'::text, 'verify'::text, 'reject'::text,
            'chain'::text, 'detach'::text, 'return'::text, 'curve_retire'::text,
            'rollback'::text]));

CREATE OR REPLACE FUNCTION public.reading_decisions_project() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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
                ELSIF NEW.kind = 'curve_retire' THEN
                    UPDATE readings SET calibration_id = (NEW.new ->> 'calibration_id')::uuid
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
            $$;

"#;

pub const DOWN: &str = r#"
    ALTER TABLE public.sensor_calibrations
        DROP COLUMN IF EXISTS retired_at,
        DROP COLUMN IF EXISTS retired_by,
        DROP COLUMN IF EXISTS retired_reason;

    ALTER TABLE public.standard_curves
        DROP COLUMN IF EXISTS retired_at,
        DROP COLUMN IF EXISTS retired_by,
        DROP COLUMN IF EXISTS retired_reason;

    DELETE FROM public.reading_decisions WHERE kind = 'curve_retire';
    ALTER TABLE public.reading_decisions
        DROP CONSTRAINT IF EXISTS reading_decisions_kind_check;
    ALTER TABLE public.reading_decisions
        ADD CONSTRAINT reading_decisions_kind_check CHECK (kind = ANY (ARRAY[
            'flag'::text, 'unflag'::text, 'withdraw'::text, 'reassert'::text, 'curve'::text,
            'calibration_pin'::text, 'instrument_pin'::text, 'slot_move'::text,
            'value_correction'::text, 'unverified_entry'::text, 'verify'::text, 'reject'::text,
            'chain'::text, 'detach'::text, 'return'::text, 'rollback'::text]));

CREATE OR REPLACE FUNCTION public.reading_decisions_project() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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
            $$;

"#;

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
