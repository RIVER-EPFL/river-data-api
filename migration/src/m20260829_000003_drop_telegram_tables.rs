use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            r#"
            DROP TABLE IF EXISTS telegram_command_audit;
            DROP TABLE IF EXISTS telegram_identities;
            "#,
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Restores the pre-drop schema (columns from m20260615, m20260814_000004 and
        // m20260817_000003) so the earlier telegram migrations remain replayable; the dropped
        // rows are gone.
        let db = manager.get_connection();
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
                updated_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                expiry_exempt        BOOLEAN NOT NULL DEFAULT FALSE,
                last_attested_at     TIMESTAMPTZ,
                telegram_user_id     BIGINT
            );
            CREATE TABLE IF NOT EXISTS telegram_command_audit (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                chat_id BIGINT NOT NULL,
                chat_type TEXT,
                identity_id UUID REFERENCES telegram_identities(id) ON DELETE SET NULL,
                keycloak_sub TEXT,
                command TEXT NOT NULL,
                outcome TEXT NOT NULL,
                detail TEXT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            );
            "#,
        )
        .await?;
        Ok(())
    }
}
