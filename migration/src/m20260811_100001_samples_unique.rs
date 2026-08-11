use sea_orm_migration::prelude::*;

/// Samples uniqueness + lifecycle hardening.
///
/// - Unique index on samples(site_id, parameter_id, collected_at) so replicate groups resolve to
///   exactly one sample and writers can use `ON CONFLICT DO NOTHING` find-or-create.
/// - Before creating the index, duplicate rows for the same key are collapsed: readings referencing
///   a duplicate are repointed at the keeper (preferring a row readings already reference), then
///   the duplicates are deleted.
/// - `refresh_sample_aggregate` treats the nullable is_flagged as "not flagged" via
///   `is_flagged IS NOT TRUE`, and DELETEs a sample once no readings (any flag state) reference it
///   instead of leaving an n=0 tombstone. A sample with only-flagged readings keeps its row
///   (stats NULL, n=0) because readings still reference it.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Repointing readings can touch compressed chunks on a long-lived deployment.
        db.execute_unprepared(
            "SET timescaledb.max_tuples_decompressed_per_dml_transaction = 0",
        )
        .await
        .ok();

        db.execute_unprepared(
            r#"
            WITH ranked AS (
                SELECT id,
                       FIRST_VALUE(id) OVER (
                           PARTITION BY site_id, parameter_id, collected_at
                           ORDER BY (EXISTS (
                               SELECT 1 FROM readings r WHERE r.sample_id = samples.id
                           )) DESC, created_at ASC NULLS LAST, id
                       ) AS keep_id
                FROM samples
            ),
            dups AS (
                SELECT id, keep_id FROM ranked WHERE id <> keep_id
            ),
            repointed AS (
                UPDATE readings r
                SET sample_id = d.keep_id
                FROM dups d
                WHERE r.sample_id = d.id
                RETURNING r.sample_id
            )
            DELETE FROM samples s USING dups d WHERE s.id = d.id;
            "#,
        )
        .await?;

        db.execute_unprepared(
            "RESET timescaledb.max_tuples_decompressed_per_dml_transaction",
        )
        .await
        .ok();

        db.execute_unprepared(
            r#"
            CREATE UNIQUE INDEX IF NOT EXISTS samples_site_param_time_uniq
                ON samples (site_id, parameter_id, collected_at);
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
                ) a
                WHERE s.id = target_sample_id;
            END;
            $$ LANGUAGE plpgsql;
            "#,
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared("DROP INDEX IF EXISTS samples_site_param_time_uniq")
            .await?;

        // Restore the m20260420 function body (is_flagged = FALSE, no zero-ref delete).
        db.execute_unprepared(
            r#"
            CREATE OR REPLACE FUNCTION refresh_sample_aggregate(target_sample_id UUID)
            RETURNS void AS $$
            BEGIN
                IF target_sample_id IS NULL THEN
                    RETURN;
                END IF;

                PERFORM pg_advisory_xact_lock(
                    hashtextextended(target_sample_id::text, 0)
                );

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
                      AND is_flagged = FALSE
                ) a
                WHERE s.id = target_sample_id;
            END;
            $$ LANGUAGE plpgsql;
            "#,
        )
        .await?;

        Ok(())
    }
}
