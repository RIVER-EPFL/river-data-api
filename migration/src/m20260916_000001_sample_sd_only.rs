use sea_orm_migration::prelude::*;

/// A replicate group's standard deviation is the sample sd (n-1), R's `sd()` in every portal.
///
/// Drops the divisor choice: `site_parameters.sd_estimator`, `samples.sd_estimator`,
/// `sd_estimator_source` and `stdev_population`, their CHECKs and index. `samples.stdev` becomes a
/// copy of `stdev_sample`, which the trigger writes and a cutover restore carries. A stored tool
/// manifest loses its outputs' `sd_estimator` key; a version keeps its `content_hash`, which is the
/// identity its past runs recorded.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
ALTER TABLE public.samples DROP COLUMN stdev;
DROP INDEX IF EXISTS public.idx_samples_sd_estimator_default;
ALTER TABLE public.samples
    DROP CONSTRAINT samples_sd_estimator_check,
    DROP CONSTRAINT samples_sd_estimator_source_check,
    DROP COLUMN sd_estimator,
    DROP COLUMN sd_estimator_source,
    DROP COLUMN stdev_population,
    ADD COLUMN stdev double precision GENERATED ALWAYS AS (stdev_sample) STORED;

ALTER TABLE public.site_parameters
    DROP CONSTRAINT site_parameters_sd_estimator_check,
    DROP COLUMN sd_estimator;

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
    SET mean         = a.mean,
        stdev_sample = a.stdev_samp,
        median       = a.median,
        n            = COALESCE(a.n, 0),
        min_value    = a.min_value,
        max_value    = a.max_value,
        updated_at   = NOW()
    FROM (
        SELECT
            AVG(COALESCE(calibrated_value, raw_value))         AS mean,
            STDDEV_SAMP(COALESCE(calibrated_value, raw_value)) AS stdev_samp,
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

UPDATE public.tool_script_versions v
   SET manifest = jsonb_set(
           v.manifest,
           '{outputs}',
           (SELECT jsonb_agg(o - 'sd_estimator' ORDER BY i)
              FROM jsonb_array_elements(v.manifest->'outputs') WITH ORDINALITY AS e(o, i)))
 WHERE jsonb_typeof(v.manifest->'outputs') = 'array'
   AND EXISTS (SELECT 1 FROM jsonb_array_elements(v.manifest->'outputs') o
                WHERE o ? 'sd_estimator');
";

const DOWN: &str = r"
ALTER TABLE public.site_parameters
    ADD COLUMN sd_estimator text,
    ADD CONSTRAINT site_parameters_sd_estimator_check
        CHECK (sd_estimator IS NULL OR sd_estimator = ANY (ARRAY['sample'::text, 'population'::text]));

ALTER TABLE public.samples DROP COLUMN stdev;
ALTER TABLE public.samples
    ADD COLUMN sd_estimator text DEFAULT 'sample'::text NOT NULL,
    ADD COLUMN sd_estimator_source text DEFAULT 'default'::text NOT NULL,
    ADD COLUMN stdev_population double precision,
    ADD CONSTRAINT samples_sd_estimator_check
        CHECK (sd_estimator = ANY (ARRAY['sample'::text, 'population'::text])),
    ADD CONSTRAINT samples_sd_estimator_source_check
        CHECK (sd_estimator_source = ANY (ARRAY['default'::text, 'slot'::text, 'sample'::text, 'stream'::text, 'tool'::text]));
ALTER TABLE public.samples
    ADD COLUMN stdev double precision GENERATED ALWAYS AS (
        CASE WHEN sd_estimator = 'population'::text THEN stdev_population ELSE stdev_sample END
    ) STORED;
CREATE INDEX idx_samples_sd_estimator_default ON public.samples USING btree (site_id, parameter_id)
    WHERE (sd_estimator_source = 'default'::text);

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
";

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
