use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // A linked chat renewed itself: `last_verified_at` moves on delivery, so an operator who
        // receives a daily alarm held a credential that never lapsed without them doing anything.
        // This column is the second clock, and only an authenticated portal request resets it, so a
        // link cannot outlive proof that its owner still holds the Keycloak account.
        db.execute_unprepared(
            "ALTER TABLE telegram_identities
             ADD COLUMN IF NOT EXISTS last_attested_at TIMESTAMPTZ",
        )
        .await?;

        // The Telegram account the link belongs to, so an inbound message can be checked against
        // the user it was claimed by rather than only against the chat it arrived in.
        db.execute_unprepared(
            "ALTER TABLE telegram_identities
             ADD COLUMN IF NOT EXISTS telegram_user_id BIGINT",
        )
        .await?;

        // Existing links start attested from today: the deploy must not retroactively expire chats
        // whose owners had no way to attest before the column existed.
        db.execute_unprepared(
            "UPDATE telegram_identities SET last_attested_at = NOW()
             WHERE last_attested_at IS NULL AND telegram_chat_id IS NOT NULL",
        )
        .await?;

        // The attestation sweep scans active claimed links by this column.
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_telegram_identities_attested
             ON telegram_identities (last_attested_at)
             WHERE is_active AND telegram_chat_id IS NOT NULL",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_telegram_identities_attested")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE telegram_identities
             DROP COLUMN IF EXISTS last_attested_at,
             DROP COLUMN IF EXISTS telegram_user_id",
        )
        .await?;
        Ok(())
    }
}
