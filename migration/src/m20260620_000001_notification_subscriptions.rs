use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Data minimization: a linked chat needs only its routing fields. The person's name and email
        // are resolved live from Keycloak by `linked_keycloak_sub` at display/send time, never stored.
        // Drop the PII columns the first cut carried.
        db.execute_unprepared(
            "ALTER TABLE telegram_identities \
             DROP COLUMN IF EXISTS email, \
             DROP COLUMN IF EXISTS display_name, \
             DROP COLUMN IF EXISTS telegram_username",
        )
        .await?;

        // Per-user notification preferences, keyed by the opaque Keycloak sub. No PII — only channel
        // toggles. The email address is resolved live from Keycloak at send time, never a column here.
        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS notification_subscribers (
                id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                keycloak_sub     TEXT NOT NULL UNIQUE,
                email_enabled    BOOLEAN NOT NULL DEFAULT FALSE,
                telegram_enabled BOOLEAN NOT NULL DEFAULT TRUE,
                is_active        BOOLEAN NOT NULL DEFAULT TRUE,
                last_verified_at TIMESTAMPTZ,
                created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .await?;

        // Per-user scope opt-ins. A row with project_id/site_id/parameter_id all NULL is the "all"
        // default (seeded enabled=TRUE when a subscriber is created); more specific rows override it
        // (most-specific match wins at fan-out time).
        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS notification_subscriptions (
                id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                keycloak_sub  TEXT NOT NULL,
                project_id    UUID REFERENCES projects(id) ON DELETE CASCADE,
                site_id       UUID REFERENCES sites(id) ON DELETE CASCADE,
                parameter_id  UUID REFERENCES parameters(id) ON DELETE CASCADE,
                enabled       BOOLEAN NOT NULL DEFAULT TRUE,
                created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .await?;
        // One row per (user, scope-tuple). NULLs compare distinct in a plain unique index, so COALESCE
        // each scope column to a sentinel UUID to dedupe the all-NULL default and partial scopes.
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_notification_subscriptions_scope \
             ON notification_subscriptions ( \
               keycloak_sub, \
               COALESCE(project_id, '00000000-0000-0000-0000-000000000000'::uuid), \
               COALESCE(site_id, '00000000-0000-0000-0000-000000000000'::uuid), \
               COALESCE(parameter_id, '00000000-0000-0000-0000-000000000000'::uuid) \
             )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_notification_subscriptions_sub \
             ON notification_subscriptions (keycloak_sub)",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP TABLE IF EXISTS notification_subscriptions")
            .await?;
        db.execute_unprepared("DROP TABLE IF EXISTS notification_subscribers")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE telegram_identities \
             ADD COLUMN IF NOT EXISTS email TEXT, \
             ADD COLUMN IF NOT EXISTS display_name TEXT, \
             ADD COLUMN IF NOT EXISTS telegram_username TEXT",
        )
        .await?;
        Ok(())
    }
}
