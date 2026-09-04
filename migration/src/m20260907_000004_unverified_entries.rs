use sea_orm_migration::prelude::*;

/// An intern's entry is pending, so it carries no published statistics until someone rules on it.
///
/// `refresh_sample_aggregate` already excludes flagged and withdrawn replicates from a sample;
/// unverified replicates join them, and the update trigger learns to fire when that column moves,
/// so a verify or a reject recomputes the group the way an unflag does.
#[derive(DeriveMigrationName)]
pub struct Migration;

const FUNCTION: &str = r#"
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
    ) a
    WHERE s.id = target_sample_id;
END;
$$;
"#;

const TRIGGER: &str = r#"
DROP TRIGGER IF EXISTS trg_readings_sample_refresh_upd ON public.readings;
CREATE TRIGGER trg_readings_sample_refresh_upd AFTER UPDATE ON public.readings
    FOR EACH ROW WHEN (((old.sample_id IS DISTINCT FROM new.sample_id) OR (old.raw_value IS DISTINCT FROM new.raw_value) OR (old.calibrated_value IS DISTINCT FROM new.calibrated_value) OR (old.is_flagged IS DISTINCT FROM new.is_flagged) OR (old.withdrawn_at IS DISTINCT FROM new.withdrawn_at) OR (old.unverified IS DISTINCT FROM new.unverified)))
    EXECUTE FUNCTION public.samples_on_reading_update();
"#;

const TRIGGER_DOWN: &str = r#"
DROP TRIGGER IF EXISTS trg_readings_sample_refresh_upd ON public.readings;
CREATE TRIGGER trg_readings_sample_refresh_upd AFTER UPDATE ON public.readings
    FOR EACH ROW WHEN (((old.sample_id IS DISTINCT FROM new.sample_id) OR (old.raw_value IS DISTINCT FROM new.raw_value) OR (old.calibrated_value IS DISTINCT FROM new.calibrated_value) OR (old.is_flagged IS DISTINCT FROM new.is_flagged) OR (old.withdrawn_at IS DISTINCT FROM new.withdrawn_at)))
    EXECUTE FUNCTION public.samples_on_reading_update();
"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(FUNCTION).await?;
        db.execute_unprepared(TRIGGER).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(FUNCTION_DOWN).await?;
        db.execute_unprepared(TRIGGER_DOWN).await?;
        Ok(())
    }
}
