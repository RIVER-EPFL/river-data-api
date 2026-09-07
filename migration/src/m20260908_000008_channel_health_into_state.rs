use sea_orm_migration::prelude::*;

/// Channel health is a `notification_state` row.
///
/// `notification_state (kind, subject_key)` is already the ledger every notification trigger
/// records its last transition in, and a channel's health is that shape: `channel_health` for the
/// kind, the channel name for the subject, `healthy` or `unhealthy` for the state, and the probe's
/// time for `last_notified_at`. The one thing it did not carry is the probe's message, so `detail`
/// is added to the ledger and the one-row table goes.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = "
    ALTER TABLE public.notification_state ADD COLUMN IF NOT EXISTS detail text;

    INSERT INTO public.notification_state (kind, subject_key, state, last_notified_at, detail)
    SELECT 'channel_health', channel,
           CASE WHEN healthy THEN 'healthy' ELSE 'unhealthy' END, checked_at, detail
      FROM public.notification_channel_health
    ON CONFLICT (kind, subject_key) DO NOTHING;

    DROP TABLE IF EXISTS public.notification_channel_health;
";

const DOWN: &str = "
    CREATE TABLE IF NOT EXISTS public.notification_channel_health (
        channel    text PRIMARY KEY,
        healthy    boolean NOT NULL,
        detail     text,
        checked_at timestamptz NOT NULL DEFAULT now()
    );

    INSERT INTO public.notification_channel_health (channel, healthy, detail, checked_at)
    SELECT subject_key, state = 'healthy', detail, last_notified_at
      FROM public.notification_state WHERE kind = 'channel_health'
    ON CONFLICT (channel) DO NOTHING;

    DELETE FROM public.notification_state WHERE kind = 'channel_health';
    ALTER TABLE public.notification_state DROP COLUMN IF EXISTS detail;
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
