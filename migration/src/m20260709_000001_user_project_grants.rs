use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Per-user, per-project visibility grants. A non-admin Keycloak user (identified by their
        // stable `sub`) sees and acts only in the projects granted here; capability comes entirely
        // from their global realm role (intern < river < manager), so a grant is pure visibility —
        // there is no per-project role. Administrators are exempt (they see all) and never need a
        // row. The table is small (bounded by users × projects) and read on every non-admin request
        // through a short-TTL cache.
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS user_project_grants ( \
                 user_sub   TEXT NOT NULL, \
                 project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE, \
                 granted_by TEXT, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 PRIMARY KEY (user_sub, project_id) \
             )",
        )
        .await?;

        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_user_project_grants_project \
             ON user_project_grants (project_id)",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_user_project_grants_project")
            .await?;
        db.execute_unprepared("DROP TABLE IF EXISTS user_project_grants")
            .await?;
        Ok(())
    }
}
