use sea_orm_migration::prelude::*;

/// The standard-deviation estimator as a declared parameter specification.
///
/// A replicate group's sd can be computed with divisor n-1 (sample) or n (population). The source
/// portals used both at different times, row by row within one stream, so the convention cannot be
/// inferred from the data: it is declared per slot by a person. `site_parameters.sd_estimator` is
/// that declaration and NULL means undeclared, which is a state the audit gate and the undeclared
/// report both read, never a synonym for 'sample'.
///
/// `samples.sd_estimator` records what a group's stdev was actually computed with, and
/// `sd_estimator_source` where that came from. Existing rows read ('sample', 'default'): computed
/// with STDDEV_SAMP under no declaration, which is true of every row that predates this.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            "ALTER TABLE site_parameters
             ADD COLUMN IF NOT EXISTS sd_estimator TEXT",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE site_parameters
             DROP CONSTRAINT IF EXISTS site_parameters_sd_estimator_check",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE site_parameters
             ADD CONSTRAINT site_parameters_sd_estimator_check
             CHECK (sd_estimator IS NULL OR sd_estimator IN ('sample', 'population'))",
        )
        .await?;

        db.execute_unprepared(
            "ALTER TABLE samples
             ADD COLUMN IF NOT EXISTS sd_estimator TEXT NOT NULL DEFAULT 'sample',
             ADD COLUMN IF NOT EXISTS sd_estimator_source TEXT NOT NULL DEFAULT 'default'",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE samples
             DROP CONSTRAINT IF EXISTS samples_sd_estimator_check",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE samples
             ADD CONSTRAINT samples_sd_estimator_check
             CHECK (sd_estimator IN ('sample', 'population'))",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE samples
             DROP CONSTRAINT IF EXISTS samples_sd_estimator_source_check",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE samples
             ADD CONSTRAINT samples_sd_estimator_source_check
             CHECK (sd_estimator_source IN ('default', 'slot', 'sample', 'stream', 'tool'))",
        )
        .await?;

        // The undeclared report scans for samples still on the fallback, per slot.
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_samples_sd_estimator_default
             ON samples (site_id, parameter_id)
             WHERE sd_estimator_source = 'default'",
        )
        .await?;

        // The estimator reaches only `stdev`: both aggregates come from the pass the function
        // already makes, and the sample's own declaration picks between them. `mean` is untouched
        // and grabs are excluded from the continuous aggregates, so a declaration change needs no
        // aggregate refresh.
        db.execute_unprepared(SAMPLE_AGGREGATE_WITH_ESTIMATOR).await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(SAMPLE_AGGREGATE_SAMPLE_ONLY).await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_samples_sd_estimator_default")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE samples
             DROP COLUMN IF EXISTS sd_estimator,
             DROP COLUMN IF EXISTS sd_estimator_source",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE site_parameters DROP COLUMN IF EXISTS sd_estimator",
        )
        .await?;
        Ok(())
    }
}

const SAMPLE_AGGREGATE_WITH_ESTIMATOR: &str = r#"
CREATE OR REPLACE FUNCTION refresh_sample_aggregate(target_sample_id UUID)
RETURNS void AS $$
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
    SET mean       = a.mean,
        stdev      = CASE WHEN estimator = 'population'
                          THEN a.stdev_pop ELSE a.stdev_samp END,
        n          = COALESCE(a.n, 0),
        min_value  = a.min_value,
        max_value  = a.max_value,
        updated_at = NOW()
    FROM (
        SELECT
            AVG(COALESCE(calibrated_value, raw_value))         AS mean,
            STDDEV_SAMP(COALESCE(calibrated_value, raw_value)) AS stdev_samp,
            STDDEV_POP(COALESCE(calibrated_value, raw_value))  AS stdev_pop,
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
"#;

const SAMPLE_AGGREGATE_SAMPLE_ONLY: &str = r#"
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
          AND withdrawn_at IS NULL
    ) a
    WHERE s.id = target_sample_id;
END;
$$ LANGUAGE plpgsql;
"#;
