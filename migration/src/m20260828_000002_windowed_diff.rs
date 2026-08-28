use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Retraction is a stamp, never a delete: a source row absent from a claimed window is
        // marked withdrawn and excluded from serving, statistics and alarms, reversibly. The
        // CHECK confines withdrawal to spot rows, which is what keeps the continuous aggregates
        // (they exclude spot) structurally unreachable by a withdrawal.
        db.execute_unprepared(
            "ALTER TABLE readings
             ADD COLUMN IF NOT EXISTS withdrawn_at TIMESTAMPTZ,
             ADD COLUMN IF NOT EXISTS withdrawn_reason TEXT",
        )
        .await?;
        db.execute_unprepared(
            "DO $$ BEGIN
                ALTER TABLE readings ADD CONSTRAINT readings_withdrawn_spot_only
                    CHECK (withdrawn_at IS NULL OR measurement_type = 'spot');
            EXCEPTION WHEN duplicate_object THEN NULL; END $$",
        )
        .await?;

        // One committed record per windowed pass: what was submitted, what the diff did, and the
        // arithmetic CHECK that makes "every submitted reading is stored or reported by kind" a
        // database fact rather than a reviewer's obligation.
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS ingest_receipts (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                stream_id UUID NOT NULL REFERENCES data_streams(id) ON DELETE CASCADE,
                at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                window_from TIMESTAMPTZ,
                window_to TIMESTAMPTZ,
                submitted INT NOT NULL,
                new_rows INT NOT NULL,
                changed INT NOT NULL,
                unchanged INT NOT NULL,
                retained INT NOT NULL,
                rejected_total INT NOT NULL,
                rejected JSONB NOT NULL,
                dropped INT NOT NULL,
                withdrawn INT NOT NULL,
                changed_keys JSONB,
                braked BOOLEAN NOT NULL DEFAULT FALSE,
                brake_threshold REAL,
                CONSTRAINT receipt_arithmetic_closes
                    CHECK (submitted = new_rows + changed + unchanged + rejected_total)
            )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_ingest_receipts_stream
             ON ingest_receipts (stream_id, at DESC)",
        )
        .await?;

        // The sample statistics exclude withdrawn readings, exactly as they exclude flagged ones.
        // `total_refs` deliberately still counts every referencing row, withdrawn included: the
        // samples row (with its operator-authored label and notes) survives a full withdrawal as
        // n = 0, so a later honest window that clears the stamps restores the statistics rather
        // than re-minting the sample. Serving requires n > 0 on the spot arm, so a fully
        // withdrawn instant is absent from results, never present with a substituted value.
        db.execute_unprepared(
            r#"
            CREATE OR REPLACE FUNCTION refresh_sample_aggregate(target_sample_id UUID)
            RETURNS void AS $$
            DECLARE
                total_refs BIGINT;
            BEGIN
                IF target_sample_id IS NULL THEN
                    RETURN;
                END IF;

                -- Serialize concurrent refreshes for the same sample.
                PERFORM pg_advisory_xact_lock(
                    hashtextextended(target_sample_id::text, 0)
                );

                SELECT COUNT(*) INTO total_refs
                FROM readings
                WHERE sample_id = target_sample_id;

                IF total_refs = 0 THEN
                    DELETE FROM samples WHERE id = target_sample_id;
                    RETURN;
                END IF;

                UPDATE samples s
                SET mean       = a.mean,
                    stdev      = a.stdev,
                    n          = COALESCE(a.n, 0),
                    min_value  = a.min_value,
                    max_value  = a.max_value,
                    updated_at = NOW()
                FROM (
                    SELECT
                        AVG(COALESCE(calibrated_value, raw_value))         AS mean,
                        STDDEV_SAMP(COALESCE(calibrated_value, raw_value)) AS stdev,
                        COUNT(*)::INTEGER                                   AS n,
                        MIN(COALESCE(calibrated_value, raw_value))         AS min_value,
                        MAX(COALESCE(calibrated_value, raw_value))         AS max_value
                    FROM readings
                    WHERE sample_id = target_sample_id
                      AND is_flagged IS NOT TRUE
                      AND withdrawn_at IS NULL
                ) a
                WHERE s.id = target_sample_id;
            END;
            $$ LANGUAGE plpgsql;
            "#,
        )
        .await?;

        // A withdrawal stamp must refresh the statistics like a flag does.
        db.execute_unprepared(
            r#"
            DROP TRIGGER IF EXISTS trg_readings_sample_refresh_upd ON readings;
            CREATE TRIGGER trg_readings_sample_refresh_upd
                AFTER UPDATE ON readings
                FOR EACH ROW
                WHEN (
                       OLD.sample_id       IS DISTINCT FROM NEW.sample_id
                    OR OLD.raw_value        IS DISTINCT FROM NEW.raw_value
                    OR OLD.calibrated_value IS DISTINCT FROM NEW.calibrated_value
                    OR OLD.is_flagged       IS DISTINCT FROM NEW.is_flagged
                    OR OLD.withdrawn_at     IS DISTINCT FROM NEW.withdrawn_at
                )
                EXECUTE FUNCTION samples_on_reading_update();
            "#,
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            r#"
            DROP TRIGGER IF EXISTS trg_readings_sample_refresh_upd ON readings;
            CREATE TRIGGER trg_readings_sample_refresh_upd
                AFTER UPDATE ON readings
                FOR EACH ROW
                WHEN (
                       OLD.sample_id       IS DISTINCT FROM NEW.sample_id
                    OR OLD.raw_value        IS DISTINCT FROM NEW.raw_value
                    OR OLD.calibrated_value IS DISTINCT FROM NEW.calibrated_value
                    OR OLD.is_flagged       IS DISTINCT FROM NEW.is_flagged
                )
                EXECUTE FUNCTION samples_on_reading_update();
            "#,
        )
        .await?;
        db.execute_unprepared(
            r#"
            CREATE OR REPLACE FUNCTION refresh_sample_aggregate(target_sample_id UUID)
            RETURNS void AS $$
            DECLARE
                total_refs BIGINT;
            BEGIN
                IF target_sample_id IS NULL THEN
                    RETURN;
                END IF;
                PERFORM pg_advisory_xact_lock(
                    hashtextextended(target_sample_id::text, 0)
                );
                SELECT COUNT(*) INTO total_refs
                FROM readings
                WHERE sample_id = target_sample_id;
                IF total_refs = 0 THEN
                    DELETE FROM samples WHERE id = target_sample_id;
                    RETURN;
                END IF;
                UPDATE samples s
                SET mean       = a.mean,
                    stdev      = a.stdev,
                    n          = COALESCE(a.n, 0),
                    min_value  = a.min_value,
                    max_value  = a.max_value,
                    updated_at = NOW()
                FROM (
                    SELECT
                        AVG(COALESCE(calibrated_value, raw_value))         AS mean,
                        STDDEV_SAMP(COALESCE(calibrated_value, raw_value)) AS stdev,
                        COUNT(*)::INTEGER                                   AS n,
                        MIN(COALESCE(calibrated_value, raw_value))         AS min_value,
                        MAX(COALESCE(calibrated_value, raw_value))         AS max_value
                    FROM readings
                    WHERE sample_id = target_sample_id
                      AND is_flagged IS NOT TRUE
                ) a
                WHERE s.id = target_sample_id;
            END;
            $$ LANGUAGE plpgsql;
            "#,
        )
        .await?;
        db.execute_unprepared("DROP TABLE IF EXISTS ingest_receipts").await?;
        db.execute_unprepared(
            "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0;",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE readings
             DROP CONSTRAINT IF EXISTS readings_withdrawn_spot_only,
             DROP COLUMN IF EXISTS withdrawn_at,
             DROP COLUMN IF EXISTS withdrawn_reason",
        )
        .await?;
        Ok(())
    }
}
