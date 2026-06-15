use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Outbox columns on alarm_events: the notification dispatcher claims rows where the relevant
        // stamp is NULL, sends, then stamps. `notified_at` covers the open notification,
        // `resolution_notified_at` the resolve notification. A re-raised breach is a fresh row, so it
        // starts unstamped and notifies again without extra bookkeeping.
        db.execute_unprepared(
            "ALTER TABLE alarm_events \
             ADD COLUMN IF NOT EXISTS notified_at TIMESTAMPTZ, \
             ADD COLUMN IF NOT EXISTS resolution_notified_at TIMESTAMPTZ",
        )
        .await?;

        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_alarm_events_pending_open \
             ON alarm_events (started_at) WHERE notified_at IS NULL AND resolved_at IS NULL",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_alarm_events_pending_resolve \
             ON alarm_events (resolved_at) \
             WHERE resolved_at IS NOT NULL AND resolution_notified_at IS NULL",
        )
        .await?;

        // Audit trail of every delivery attempt, one row per (event, channel, recipient). Backs the
        // UI feed and lets the dispatcher avoid double-sending sync/stale/battery alerts (which have no
        // alarm_event_id) by watermarking on kind + created_at.
        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS notification_log (
                id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                alarm_event_id  UUID REFERENCES alarm_events(id) ON DELETE SET NULL,
                kind            TEXT NOT NULL,
                channel         TEXT NOT NULL,
                recipient       TEXT NOT NULL,
                status          TEXT NOT NULL,
                error           TEXT,
                created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_notification_log_created \
             ON notification_log (created_at DESC)",
        )
        .await?;

        // A Telegram chat bound to a Keycloak user. The chat is a delivery address only — the
        // effective role is always resolved live from linked_keycloak_sub, never stored here. A row is
        // created pending (chat_id NULL, link_code set) and claimed when the user sends /start <code>.
        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS telegram_identities (
                id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                linked_keycloak_sub  TEXT NOT NULL,
                email                TEXT,
                display_name         TEXT,
                telegram_chat_id     BIGINT UNIQUE,
                telegram_username    TEXT,
                receive_alerts       BOOLEAN NOT NULL DEFAULT TRUE,
                is_active            BOOLEAN NOT NULL DEFAULT TRUE,
                link_code            TEXT UNIQUE,
                link_code_expires_at TIMESTAMPTZ,
                last_verified_at     TIMESTAMPTZ,
                created_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at           TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .await?;
        // Revoke hook + reconciliation sweep look up identities by their linked Keycloak user.
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_telegram_identities_sub \
             ON telegram_identities (linked_keycloak_sub)",
        )
        .await?;

        // Per-slot notification suppression set by /mute. One row per (site, parameter); re-muting
        // upserts the expiry. expires_at NULL means permanent until /unmute.
        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS notification_mutes (
                id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                site_id       UUID NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
                parameter_id  UUID NOT NULL REFERENCES parameters(id),
                expires_at    TIMESTAMPTZ,
                created_by    TEXT,
                created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_notification_mutes_slot \
             ON notification_mutes (site_id, parameter_id)",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP TABLE IF EXISTS notification_mutes")
            .await?;
        db.execute_unprepared("DROP TABLE IF EXISTS telegram_identities")
            .await?;
        db.execute_unprepared("DROP TABLE IF EXISTS notification_log")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE alarm_events \
             DROP COLUMN IF EXISTS notified_at, \
             DROP COLUMN IF EXISTS resolution_notified_at",
        )
        .await?;
        Ok(())
    }
}
