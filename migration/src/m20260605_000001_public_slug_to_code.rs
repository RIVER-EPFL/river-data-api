use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"
            DROP INDEX IF EXISTS projects_public_slug_idx;
            ALTER TABLE projects RENAME COLUMN public_slug TO public_code;
            CREATE UNIQUE INDEX IF NOT EXISTS projects_public_code_idx ON projects (public_code) WHERE public_code IS NOT NULL;
            ALTER TABLE sites RENAME COLUMN public_slug TO public_code;
            "#,
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"
            ALTER TABLE sites RENAME COLUMN public_code TO public_slug;
            DROP INDEX IF EXISTS projects_public_code_idx;
            ALTER TABLE projects RENAME COLUMN public_code TO public_slug;
            CREATE UNIQUE INDEX IF NOT EXISTS projects_public_slug_idx ON projects (public_slug) WHERE public_slug IS NOT NULL;
            "#,
        )
        .await?;

        Ok(())
    }
}
