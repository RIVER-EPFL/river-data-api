use sea_orm_migration::prelude::*;

/// The median, and both standard deviations, on every replicate group.
///
/// `samples.stdev` holds the one divisor the slot declares, so the other is unreadable without
/// declaring the slot first, which is exactly the decision an audit hold is asking about. The
/// trigger already computes both in the pass it makes; this stores both, and adds the median the
/// lab asks for beside them. `stdev` stays the declared one, so nothing that reads it changes.
///
/// No aggregate refresh is needed: grabs are excluded from the continuous aggregates.
#[derive(DeriveMigrationName)]
pub struct Migration;

const ADD_COLUMNS: &str = r"
ALTER TABLE samples ADD COLUMN IF NOT EXISTS median double precision;
ALTER TABLE samples ADD COLUMN IF NOT EXISTS stdev_sample double precision;
ALTER TABLE samples ADD COLUMN IF NOT EXISTS stdev_population double precision;
";

/// The trigger body, with the three columns added to the pass it already makes.
const REFRESH_FUNCTION: &str = r"
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
";

/// Fill the three columns on the rows that predate them, by the same arithmetic.
pub const BACKFILL: &str = r"
UPDATE samples s
   SET stdev_sample     = a.stdev_samp,
       stdev_population = a.stdev_pop,
       median           = a.median
  FROM (
    SELECT sample_id,
           STDDEV_SAMP(COALESCE(calibrated_value, raw_value)) AS stdev_samp,
           STDDEV_POP(COALESCE(calibrated_value, raw_value))  AS stdev_pop,
           PERCENTILE_CONT(0.5) WITHIN GROUP (
               ORDER BY COALESCE(calibrated_value, raw_value)) AS median
      FROM readings
     WHERE sample_id IS NOT NULL
       AND is_flagged IS NOT TRUE
       AND withdrawn_at IS NULL
     GROUP BY sample_id
  ) a
 WHERE s.id = a.sample_id
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(ADD_COLUMNS).await?;
        db.execute_unprepared(REFRESH_FUNCTION).await?;
        db.execute_unprepared(BACKFILL).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE samples DROP COLUMN IF EXISTS median, \
                 DROP COLUMN IF EXISTS stdev_sample, \
                 DROP COLUMN IF EXISTS stdev_population;",
            )
            .await?;
        Ok(())
    }
}
