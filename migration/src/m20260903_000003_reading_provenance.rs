use sea_orm_migration::prelude::*;

/// Provenance is a property of the reading, not of the group it happens to sit in.
///
/// A `samples` row exists for two or more spot readings at an instant, so a single measurement has
/// no group to carry its story on. The blob a tool save builds, the operator's label and notes and
/// the author move down onto `readings`, and `samples` keeps statistics only. The n = 1 rows the
/// old rule minted are unstamped and reaped once their story has been copied down.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "ALTER TABLE readings
               ADD COLUMN IF NOT EXISTS provenance JSONB,
               ADD COLUMN IF NOT EXISTS label TEXT,
               ADD COLUMN IF NOT EXISTS notes TEXT,
               ADD COLUMN IF NOT EXISTS created_by TEXT",
        )
        .await?;

        // The chain executor looks a tool's last blob up by (site, instant, tool); the partial
        // index keeps that off the hypertable's full chunk set.
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_readings_provenance_site_time \
             ON readings (site_id, time) WHERE provenance IS NOT NULL",
        )
        .await?;

        db.execute_unprepared("SET timescaledb.max_tuples_decompressed_per_dml_transaction = 0")
            .await?;
        db.execute_unprepared(
            "UPDATE readings r
                SET provenance = s.provenance,
                    label      = s.label,
                    notes      = s.notes,
                    created_by = s.created_by
               FROM samples s
              WHERE s.id = r.sample_id
                AND (s.provenance IS NOT NULL OR s.label IS NOT NULL
                     OR s.notes IS NOT NULL OR s.created_by IS NOT NULL)",
        )
        .await?;

        // How many readings share the instant is the rule, not how many of them are unflagged: a
        // group whose replicates are all flagged carries n = 0 and is still a group. Unstamping is
        // what reaps it; the DELETE covers a row nothing referenced to begin with.
        db.execute_unprepared(
            "UPDATE readings SET sample_id = NULL
              WHERE sample_id IN (SELECT s.id FROM samples s
                                   WHERE (SELECT COUNT(*) FROM readings r
                                           WHERE r.sample_id = s.id) < 2)",
        )
        .await?;
        db.execute_unprepared(
            "DELETE FROM samples s
              WHERE NOT EXISTS (SELECT 1 FROM readings r WHERE r.sample_id = s.id)",
        )
        .await?;
        db.execute_unprepared("RESET timescaledb.max_tuples_decompressed_per_dml_transaction")
            .await?;

        db.execute_unprepared(
            "ALTER TABLE samples
               DROP COLUMN IF EXISTS provenance,
               DROP COLUMN IF EXISTS label,
               DROP COLUMN IF EXISTS notes,
               DROP COLUMN IF EXISTS created_by",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "ALTER TABLE samples
               ADD COLUMN IF NOT EXISTS provenance JSONB,
               ADD COLUMN IF NOT EXISTS label TEXT,
               ADD COLUMN IF NOT EXISTS notes TEXT,
               ADD COLUMN IF NOT EXISTS created_by TEXT",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE samples s
                SET provenance = f.provenance,
                    label      = f.label,
                    notes      = f.notes,
                    created_by = f.created_by
               FROM (SELECT DISTINCT ON (sample_id) sample_id, provenance, label, notes, created_by
                       FROM readings
                      WHERE sample_id IS NOT NULL
                        AND (provenance IS NOT NULL OR label IS NOT NULL
                             OR notes IS NOT NULL OR created_by IS NOT NULL)
                      ORDER BY sample_id, replicate_index) f
              WHERE f.sample_id = s.id",
        )
        .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_readings_provenance_site_time")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE readings
               DROP COLUMN IF EXISTS provenance,
               DROP COLUMN IF EXISTS label,
               DROP COLUMN IF EXISTS notes,
               DROP COLUMN IF EXISTS created_by",
        )
        .await?;
        Ok(())
    }
}
