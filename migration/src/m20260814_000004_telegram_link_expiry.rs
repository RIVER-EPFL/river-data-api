use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // A link that nobody has used in months should lapse, so a departed collaborator's chat id
        // does not sit here receiving site data forever. `expiry_exempt` holds a link open against
        // *inactivity only* (a shared operations chat, or an account kept for a dormant field
        // season. It never shields a revoked Keycloak user: the reconcile sweep deactivates those
        // unconditionally, before it consults this column.
        db.execute_unprepared(
            "ALTER TABLE telegram_identities
             ADD COLUMN IF NOT EXISTS expiry_exempt BOOLEAN NOT NULL DEFAULT FALSE",
        )
        .await?;

        // The idle sweep orders by this column across every active identity.
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_telegram_identities_last_verified
             ON telegram_identities (last_verified_at)
             WHERE is_active AND NOT expiry_exempt",
        )
        .await?;

        // `annotations.category` is nullable in the schema but non-optional in the SeaORM model, so
        // any NULL row fails to deserialize on read. No rows carry NULL today, which is why this has
        // gone unnoticed; the plot overlay reads this table, so close it now.
        db.execute_unprepared("UPDATE annotations SET category = 'general' WHERE category IS NULL")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE annotations
             ALTER COLUMN category SET DEFAULT 'general',
             ALTER COLUMN category SET NOT NULL",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_telegram_identities_last_verified")
            .await?;
        // m20260829_000003 drops the table outright, so a rollback path may find it gone.
        db.execute_unprepared(
            "DO $$ BEGIN
                IF to_regclass('telegram_identities') IS NOT NULL THEN
                    ALTER TABLE telegram_identities DROP COLUMN IF EXISTS expiry_exempt;
                END IF;
            END $$",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE annotations
             ALTER COLUMN category DROP NOT NULL,
             ALTER COLUMN category DROP DEFAULT",
        )
        .await?;
        Ok(())
    }
}
