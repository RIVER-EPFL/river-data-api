use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Telegram carries the bot token in the request path, and a `reqwest` transport error
        // stringifies the URL it failed on. Both of these columns persist such an error, and
        // `notification_channel_health.detail` is rendered in the admin UI, so a network blip used
        // to put the token on screen. The client redacts the URL now; this clears anything written
        // before that, keeping the message and dropping only the credential.
        db.execute_unprepared(
            "UPDATE notification_channel_health
             SET detail = regexp_replace(detail, 'api\\.telegram\\.org/bot[^/[:space:])]*',
                                         'api.telegram.org/bot<redacted>', 'g')
             WHERE detail LIKE '%api.telegram.org/bot%'",
        )
        .await?;

        db.execute_unprepared(
            "UPDATE notification_log
             SET error = regexp_replace(error, 'api\\.telegram\\.org/bot[^/[:space:])]*',
                                        'api.telegram.org/bot<redacted>', 'g')
             WHERE error LIKE '%api.telegram.org/bot%'",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // A redacted secret is not recoverable, and would not be worth recovering.
        Ok(())
    }
}
