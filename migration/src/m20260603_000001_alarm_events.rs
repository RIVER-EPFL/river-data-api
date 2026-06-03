use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Persistent alarm events: one open row per (site, parameter) breach, resolved when the
        // reading returns to range. `resolved_at IS NULL` means open; the partial unique index
        // guarantees at most one open event per pair (so the sweeper's open-or-update is idempotent)
        // while keeping resolved rows as history that can re-raise.
        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS alarm_events (
                id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                site_id         UUID NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
                parameter_id    UUID NOT NULL REFERENCES parameters(id),
                severity        SMALLINT NOT NULL,
                max_severity    SMALLINT NOT NULL,
                started_at      TIMESTAMPTZ NOT NULL,
                value_at_start  DOUBLE PRECISION NOT NULL,
                last_seen_at    TIMESTAMPTZ NOT NULL,
                last_value      DOUBLE PRECISION NOT NULL,
                acknowledged_at TIMESTAMPTZ,
                acknowledged_by TEXT,
                resolved_at     TIMESTAMPTZ,
                resolved_value  DOUBLE PRECISION,
                created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .await?;

        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_alarm_events_open \
             ON alarm_events (site_id, parameter_id) WHERE resolved_at IS NULL",
        )
        .await?;

        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_alarm_events_open \
             ON alarm_events (resolved_at) WHERE resolved_at IS NULL",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS alarm_events")
            .await?;
        Ok(())
    }
}
