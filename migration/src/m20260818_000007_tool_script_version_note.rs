use sea_orm_migration::prelude::*;

/// A version records what changed and why. The activation audit says who flipped which pointer,
/// which is a different question: a version can exist for a long time before it is activated, and
/// the reason it was written is only knowable when it is written.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE tool_script_versions ADD COLUMN note TEXT")
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE tool_script_versions DROP COLUMN IF EXISTS note")
            .await?;
        Ok(())
    }
}
