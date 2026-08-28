use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"
            CREATE TABLE IF NOT EXISTS web_push_subscriptions (
                id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                keycloak_sub  TEXT NOT NULL,
                endpoint      TEXT NOT NULL UNIQUE,
                p256dh        TEXT NOT NULL,
                auth          TEXT NOT NULL,
                user_agent    TEXT,
                created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                last_success_at TIMESTAMPTZ
            );
            CREATE INDEX IF NOT EXISTS idx_wps_keycloak_sub
                ON web_push_subscriptions (keycloak_sub);
            "#,
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS web_push_subscriptions")
            .await?;
        Ok(())
    }
}
