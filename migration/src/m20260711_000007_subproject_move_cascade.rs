use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Moving a subproject to another project must carry its sites' denormalized project_id
        // along, or project grants and scoping would keep pointing at the old project. The site
        // trigger's project-changed branch previously re-derived the destination project's oldest
        // subproject unconditionally, which would detach sites from the subproject being moved —
        // so it now keeps subproject_id whenever the current subproject already belongs to the
        // new project.
        db.execute_unprepared(
            "CREATE OR REPLACE FUNCTION sites_ensure_subproject() RETURNS trigger AS $$ \
             BEGIN \
               IF TG_OP = 'UPDATE' AND NEW.subproject_id IS DISTINCT FROM OLD.subproject_id \
                  AND NEW.subproject_id IS NOT NULL THEN \
                 SELECT project_id INTO NEW.project_id FROM subprojects WHERE id = NEW.subproject_id; \
               ELSIF TG_OP = 'UPDATE' AND NEW.project_id IS DISTINCT FROM OLD.project_id THEN \
                 IF NEW.subproject_id IS NULL OR NOT EXISTS ( \
                     SELECT 1 FROM subprojects \
                     WHERE id = NEW.subproject_id AND project_id = NEW.project_id) THEN \
                   SELECT id INTO NEW.subproject_id FROM subprojects \
                     WHERE project_id = NEW.project_id ORDER BY created_at LIMIT 1; \
                 END IF; \
               ELSIF TG_OP = 'INSERT' THEN \
                 IF NEW.subproject_id IS NOT NULL THEN \
                   SELECT project_id INTO NEW.project_id FROM subprojects WHERE id = NEW.subproject_id; \
                 ELSIF NEW.project_id IS NOT NULL THEN \
                   SELECT id INTO NEW.subproject_id FROM subprojects \
                     WHERE project_id = NEW.project_id ORDER BY created_at LIMIT 1; \
                 END IF; \
               END IF; \
               RETURN NEW; \
             END; $$ LANGUAGE plpgsql",
        )
        .await?;

        db.execute_unprepared(
            "CREATE OR REPLACE FUNCTION subprojects_move_cascade() RETURNS trigger AS $$ \
             BEGIN \
               UPDATE sites SET project_id = NEW.project_id \
               WHERE subproject_id = NEW.id AND project_id IS DISTINCT FROM NEW.project_id; \
               RETURN NEW; \
             END; $$ LANGUAGE plpgsql",
        )
        .await?;
        db.execute_unprepared("DROP TRIGGER IF EXISTS subprojects_move_cascade_trg ON subprojects")
            .await?;
        db.execute_unprepared(
            "CREATE TRIGGER subprojects_move_cascade_trg AFTER UPDATE OF project_id ON subprojects \
             FOR EACH ROW WHEN (OLD.project_id IS DISTINCT FROM NEW.project_id) \
             EXECUTE FUNCTION subprojects_move_cascade()",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP TRIGGER IF EXISTS subprojects_move_cascade_trg ON subprojects")
            .await?;
        db.execute_unprepared("DROP FUNCTION IF EXISTS subprojects_move_cascade()")
            .await?;
        db.execute_unprepared(
            "CREATE OR REPLACE FUNCTION sites_ensure_subproject() RETURNS trigger AS $$ \
             BEGIN \
               IF TG_OP = 'UPDATE' AND NEW.subproject_id IS DISTINCT FROM OLD.subproject_id \
                  AND NEW.subproject_id IS NOT NULL THEN \
                 SELECT project_id INTO NEW.project_id FROM subprojects WHERE id = NEW.subproject_id; \
               ELSIF TG_OP = 'UPDATE' AND NEW.project_id IS DISTINCT FROM OLD.project_id THEN \
                 SELECT id INTO NEW.subproject_id FROM subprojects \
                   WHERE project_id = NEW.project_id ORDER BY created_at LIMIT 1; \
               ELSIF TG_OP = 'INSERT' THEN \
                 IF NEW.subproject_id IS NOT NULL THEN \
                   SELECT project_id INTO NEW.project_id FROM subprojects WHERE id = NEW.subproject_id; \
                 ELSIF NEW.project_id IS NOT NULL THEN \
                   SELECT id INTO NEW.subproject_id FROM subprojects \
                     WHERE project_id = NEW.project_id ORDER BY created_at LIMIT 1; \
                 END IF; \
               END IF; \
               RETURN NEW; \
             END; $$ LANGUAGE plpgsql",
        )
        .await?;
        Ok(())
    }
}
