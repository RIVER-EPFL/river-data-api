use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Who used the bot, when, and whether they were allowed to. Nothing here is user-authored:
        // `command` is drawn from a fixed vocabulary and unrecognised input is stored as `unknown`,
        // so an inbound message body never reaches the database. `identity_id` goes NULL rather
        // than cascading when a link is purged, because the record of the access outlives the link.
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS telegram_command_audit (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                chat_id BIGINT NOT NULL,
                chat_type TEXT,
                identity_id UUID REFERENCES telegram_identities(id) ON DELETE SET NULL,
                keycloak_sub TEXT,
                command TEXT NOT NULL,
                outcome TEXT NOT NULL,
                detail TEXT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )",
        )
        .await?;

        // The listing reads newest-first, and the retention sweep deletes by age.
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_telegram_audit_created
             ON telegram_command_audit (created_at DESC)",
        )
        .await?;
        // "What has this user done" is the question an audit trail exists to answer.
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_telegram_audit_sub
             ON telegram_command_audit (keycloak_sub, created_at DESC)",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS telegram_command_audit")
            .await?;
        Ok(())
    }
}
