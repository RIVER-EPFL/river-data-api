use sea_orm_migration::prelude::*;

/// The single definition of `refresh_sample_aggregate`, the function all three `readings` sample
/// triggers call.
///
/// Two things about it have to be stated in one place, because earlier definitions of the same
/// function disagree and whichever ran last wins. A NULL `is_flagged` reads as unflagged, so the
/// statistics are taken over `is_flagged IS NOT TRUE` rather than `is_flagged = FALSE`, which drops
/// every reading whose flag was never set. And a sample no reading references any more is removed
/// rather than left as an `n = 0` tombstone: the function maintains statistics and reaps, it never
/// creates a sample, so whether a group of readings is a sample stays the application's decision in
/// `readings::sample_groups::forms_sample`.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
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
        // The predicate this migration exists to settle: a flag that was never set counted as
        // flagged, so a sample of unflagged readings reported no statistics.
        manager
            .get_connection()
            .execute_unprepared(
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
