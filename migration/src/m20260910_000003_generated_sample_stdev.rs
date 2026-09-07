use sea_orm_migration::prelude::*;

/// `samples.stdev` is whichever divisor the row declares, so the database derives it rather than
/// the trigger storing a third copy of a number it already writes twice.
#[derive(DeriveMigrationName)]
pub struct Migration;

const GENERATED_COLUMN: &str = r"
ALTER TABLE samples DROP COLUMN stdev;
ALTER TABLE samples ADD COLUMN stdev DOUBLE PRECISION
    GENERATED ALWAYS AS (
        CASE WHEN sd_estimator = 'population' THEN stdev_population ELSE stdev_sample END
    ) STORED;
";

const PLAIN_COLUMN: &str = r"
ALTER TABLE samples DROP COLUMN stdev;
ALTER TABLE samples ADD COLUMN stdev DOUBLE PRECISION;
UPDATE samples
SET stdev = CASE WHEN sd_estimator = 'population' THEN stdev_population ELSE stdev_sample END;
";

const FUNCTION: &str = r#"
CREATE OR REPLACE FUNCTION public.refresh_sample_aggregate(target_sample_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
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
    SET mean             = a.mean,
        stdev_sample     = a.stdev_samp,
        stdev_population = a.stdev_pop,
        median           = a.median,
        n                = COALESCE(a.n, 0),
        min_value        = a.min_value,
        max_value        = a.max_value,
        updated_at       = NOW()
    FROM (
        SELECT
            AVG(COALESCE(calibrated_value, raw_value))         AS mean,
            STDDEV_SAMP(COALESCE(calibrated_value, raw_value)) AS stdev_samp,
            STDDEV_POP(COALESCE(calibrated_value, raw_value))  AS stdev_pop,
            PERCENTILE_CONT(0.5) WITHIN GROUP (
                ORDER BY COALESCE(calibrated_value, raw_value))  AS median,
            COUNT(*)::INTEGER                                   AS n,
            MIN(COALESCE(calibrated_value, raw_value))         AS min_value,
            MAX(COALESCE(calibrated_value, raw_value))         AS max_value
        FROM readings
        WHERE sample_id = target_sample_id
          AND is_flagged IS NOT TRUE
          AND withdrawn_at IS NULL
          AND unverified IS NOT TRUE
    ) a
    WHERE s.id = target_sample_id;
END;
$$;
"#;

const FUNCTION_DOWN: &str = r#"
CREATE OR REPLACE FUNCTION public.refresh_sample_aggregate(target_sample_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    total_refs BIGINT;
    estimator TEXT;
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

    SELECT s.sd_estimator INTO estimator
    FROM samples s
    WHERE s.id = target_sample_id;

    UPDATE samples s
    SET mean             = a.mean,
        stdev            = CASE WHEN estimator = 'population'
                                THEN a.stdev_pop ELSE a.stdev_samp END,
        stdev_sample     = a.stdev_samp,
        stdev_population = a.stdev_pop,
        median           = a.median,
        n                = COALESCE(a.n, 0),
        min_value        = a.min_value,
        max_value        = a.max_value,
        updated_at       = NOW()
    FROM (
        SELECT
            AVG(COALESCE(calibrated_value, raw_value))         AS mean,
            STDDEV_SAMP(COALESCE(calibrated_value, raw_value)) AS stdev_samp,
            STDDEV_POP(COALESCE(calibrated_value, raw_value))  AS stdev_pop,
            PERCENTILE_CONT(0.5) WITHIN GROUP (
                ORDER BY COALESCE(calibrated_value, raw_value))  AS median,
            COUNT(*)::INTEGER                                   AS n,
            MIN(COALESCE(calibrated_value, raw_value))         AS min_value,
            MAX(COALESCE(calibrated_value, raw_value))         AS max_value
        FROM readings
        WHERE sample_id = target_sample_id
          AND is_flagged IS NOT TRUE
          AND withdrawn_at IS NULL
          AND unverified IS NOT TRUE
    ) a
    WHERE s.id = target_sample_id;
END;
$$;
"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(FUNCTION).await?;
        db.execute_unprepared(GENERATED_COLUMN).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(PLAIN_COLUMN).await?;
        db.execute_unprepared(FUNCTION_DOWN).await?;
        Ok(())
    }
}
