use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            r#"ALTER TABLE reprocessing_jobs ALTER COLUMN sensor_id DROP NOT NULL"#,
        )
        .await?;
        db.execute_unprepared(
            r#"ALTER TABLE reprocessing_jobs ADD COLUMN IF NOT EXISTS total INTEGER"#,
        )
        .await?;
        db.execute_unprepared(
            r#"ALTER TABLE reprocessing_jobs ADD COLUMN IF NOT EXISTS progress INTEGER"#,
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            r#"ALTER TABLE reprocessing_jobs DROP COLUMN IF EXISTS progress"#,
        )
        .await?;
        db.execute_unprepared(
            r#"ALTER TABLE reprocessing_jobs DROP COLUMN IF EXISTS total"#,
        )
        .await?;
        db.execute_unprepared(
            r#"ALTER TABLE reprocessing_jobs ALTER COLUMN sensor_id SET NOT NULL"#,
        )
        .await?;
        Ok(())
    }
}
