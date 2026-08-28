use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // The portal's wide `data` row reborn as an entity: one row per (station, staged
        // timestamp) visit. Readings attach through a nullable FK; replicates share the
        // collection timestamp and attach to the same event.
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS collection_events (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                site_id UUID NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
                collected_at TIMESTAMPTZ NOT NULL,
                source TEXT NOT NULL DEFAULT 'manual'
                    CHECK (source IN ('manual', 'portal_sync')),
                created_by TEXT,
                notes TEXT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ,
                UNIQUE (site_id, collected_at)
            )",
        )
        .await?;

        db.execute_unprepared(
            "ALTER TABLE readings ADD COLUMN IF NOT EXISTS collection_event_id UUID",
        )
        .await?;
        // An event is never deleted in normal operation; SET NULL covers site cascade.
        db.execute_unprepared(
            "DO $$ BEGIN
                ALTER TABLE readings ADD CONSTRAINT fk_readings_collection_event
                    FOREIGN KEY (collection_event_id) REFERENCES collection_events(id)
                    ON DELETE SET NULL;
            EXCEPTION WHEN duplicate_object THEN NULL; END $$",
        )
        .await?;

        // Backfill: every attributed spot instant is a collection event. Source is derived from
        // the streams feeding the instant: anything that is not one of our own writer-created
        // stream kinds came through a sync service.
        db.execute_unprepared(
            "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0;",
        )
        .await?;
        db.execute_unprepared(
            "INSERT INTO collection_events (site_id, collected_at, source)
             SELECT r.site_id, r.time,
                    CASE WHEN bool_or(ds.source_system NOT IN ('api', 'grab_sample', 'csv', 'csv_import'))
                         THEN 'portal_sync' ELSE 'manual' END
             FROM readings r
             JOIN data_streams ds ON ds.id = r.stream_id
             WHERE r.measurement_type = 'spot' AND r.site_id IS NOT NULL
             GROUP BY r.site_id, r.time
             ON CONFLICT (site_id, collected_at) DO NOTHING",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE readings r SET collection_event_id = ce.id
             FROM collection_events ce
             WHERE r.measurement_type = 'spot' AND r.site_id IS NOT NULL
               AND r.collection_event_id IS NULL
               AND ce.site_id = r.site_id AND ce.collected_at = r.time",
        )
        .await?;

        // A site_parameter minted mechanically (first tool save at a site) rather than by a
        // person, awaiting an operator's look.
        db.execute_unprepared(
            "ALTER TABLE site_parameters
             ADD COLUMN IF NOT EXISTS needs_review BOOLEAN NOT NULL DEFAULT FALSE",
        )
        .await?;

        // context: the run's resolved calculation context (site, collection instant, resolved
        // station and event inputs). source: which path minted the run (interactive calculate,
        // CSV tool entry, chain recompute, event audit probe).
        db.execute_unprepared(
            "ALTER TABLE tool_runs
             ADD COLUMN IF NOT EXISTS context JSONB,
             ADD COLUMN IF NOT EXISTS source TEXT NOT NULL DEFAULT 'interactive'",
        )
        .await?;

        // The portal's Check gate: one row per screening of entered values against the site's
        // seasonal distribution. A save naming a check is validated against this row's entries,
        // so an edit after checking cannot ride an old check.
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS seasonal_checks (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                site_id UUID NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
                checked_time TIMESTAMPTZ NOT NULL,
                entries JSONB NOT NULL,
                created_by TEXT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_seasonal_checks_site
             ON seasonal_checks (site_id, created_at)",
        )
        .await?;

        // The review queue widens to carry event-audit findings (missing or stale tool outputs at
        // a collection event) beside the replicate-statistics holds. A finding has no stream: it
        // is keyed on (site, output parameter, collected_at, tool).
        db.execute_unprepared(
            "ALTER TABLE replicate_audit_holds ALTER COLUMN stream_id DROP NOT NULL",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE replicate_audit_holds
             ADD COLUMN IF NOT EXISTS kind TEXT NOT NULL DEFAULT 'replicate_stats',
             ADD COLUMN IF NOT EXISTS site_id UUID REFERENCES sites(id) ON DELETE CASCADE,
             ADD COLUMN IF NOT EXISTS parameter_id UUID,
             ADD COLUMN IF NOT EXISTS tool TEXT",
        )
        .await?;
        db.execute_unprepared(
            "DO $$ BEGIN
                ALTER TABLE replicate_audit_holds ADD CONSTRAINT audit_hold_subject
                    CHECK (stream_id IS NOT NULL
                           OR (kind <> 'replicate_stats' AND site_id IS NOT NULL
                               AND parameter_id IS NOT NULL));
            EXCEPTION WHEN duplicate_object THEN NULL; END $$",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS replicate_audit_holds_event_live_uniq
             ON replicate_audit_holds (kind, site_id, parameter_id, group_time)
             WHERE stream_id IS NULL AND status = 'pending'",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "DELETE FROM replicate_audit_holds WHERE stream_id IS NULL",
        )
        .await?;
        db.execute_unprepared(
            "DROP INDEX IF EXISTS replicate_audit_holds_event_live_uniq;
             ALTER TABLE replicate_audit_holds
                 DROP CONSTRAINT IF EXISTS audit_hold_subject,
                 DROP COLUMN IF EXISTS kind,
                 DROP COLUMN IF EXISTS site_id,
                 DROP COLUMN IF EXISTS parameter_id,
                 DROP COLUMN IF EXISTS tool;
             ALTER TABLE replicate_audit_holds ALTER COLUMN stream_id SET NOT NULL",
        )
        .await?;
        db.execute_unprepared("DROP TABLE IF EXISTS seasonal_checks").await?;
        db.execute_unprepared(
            "ALTER TABLE tool_runs DROP COLUMN IF EXISTS context, DROP COLUMN IF EXISTS source",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE site_parameters DROP COLUMN IF EXISTS needs_review",
        )
        .await?;
        db.execute_unprepared(
            "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0;",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE readings DROP COLUMN IF EXISTS collection_event_id",
        )
        .await?;
        db.execute_unprepared("DROP TABLE IF EXISTS collection_events").await?;
        Ok(())
    }
}
