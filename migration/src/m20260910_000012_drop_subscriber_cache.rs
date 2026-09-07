use sea_orm_migration::prelude::*;

/// A subscriber row is the person's own preferences and nothing else (Q82).
///
/// `is_active` cached what the authorizer resolves live in the same loop that wrote it, so it
/// could disagree with the authority between reconcile ticks; deleting the revoked person's
/// `web_push_subscriptions` rows is the whole mechanism. `last_verified_at` was written nowhere
/// and read nowhere.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = "
    ALTER TABLE public.notification_subscribers
        DROP COLUMN IF EXISTS is_active,
        DROP COLUMN IF EXISTS last_verified_at;
";

pub const DOWN: &str = "
    ALTER TABLE public.notification_subscribers
        ADD COLUMN IF NOT EXISTS is_active boolean NOT NULL DEFAULT true,
        ADD COLUMN IF NOT EXISTS last_verified_at timestamptz;
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(UP).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(DOWN).await?;
        Ok(())
    }
}
