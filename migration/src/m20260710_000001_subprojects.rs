use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Subprojects sit between projects and sites: every site belongs to exactly one. The level is
        // mandatory, but enforced by a trigger (below) rather than a NOT NULL column — sites are
        // created through several paths (CrudCrate, sync discovery, raw SQL) and the trigger derives a
        // default subproject for any that omit one, so no path has to be taught about the new column.
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS subprojects ( \
                 id          UUID PRIMARY KEY DEFAULT gen_random_uuid(), \
                 project_id  UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE, \
                 name        VARCHAR(64) NOT NULL, \
                 description TEXT, \
                 created_at  TIMESTAMPTZ NOT NULL DEFAULT now() \
             )",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS subprojects_project_name_idx \
             ON subprojects (project_id, lower(name))",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS subprojects_project_idx ON subprojects (project_id)",
        )
        .await?;

        // Rescue any orphan sites (nullable project_id today) into an auto-created "Unassigned"
        // project so every site has a project — the prerequisite for a subproject.
        db.execute_unprepared(
            "DO $$ \
             DECLARE unassigned UUID; \
             BEGIN \
               IF EXISTS (SELECT 1 FROM sites WHERE project_id IS NULL) THEN \
                 SELECT id INTO unassigned FROM projects WHERE name = 'Unassigned' LIMIT 1; \
                 IF unassigned IS NULL THEN \
                   INSERT INTO projects (id, name, description, data_source) \
                   VALUES (gen_random_uuid(), 'Unassigned', 'Auto-created for sites without a project', 'system') \
                   RETURNING id INTO unassigned; \
                 END IF; \
                 UPDATE sites SET project_id = unassigned WHERE project_id IS NULL; \
               END IF; \
             END $$",
        )
        .await?;

        // One default subproject per existing project (name mirrors the project).
        db.execute_unprepared(
            "INSERT INTO subprojects (project_id, name) \
             SELECT p.id, p.name FROM projects p \
             WHERE NOT EXISTS (SELECT 1 FROM subprojects s WHERE s.project_id = p.id)",
        )
        .await?;

        db.execute_unprepared(
            "ALTER TABLE sites ADD COLUMN IF NOT EXISTS subproject_id UUID REFERENCES subprojects(id)",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE sites st \
             SET subproject_id = ( \
                 SELECT s.id FROM subprojects s WHERE s.project_id = st.project_id \
                 ORDER BY s.created_at LIMIT 1) \
             WHERE subproject_id IS NULL AND project_id IS NOT NULL",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS sites_subproject_idx ON sites (subproject_id)",
        )
        .await?;

        // Every new project gets a default subproject, so the site trigger can always find one.
        db.execute_unprepared(
            "CREATE OR REPLACE FUNCTION projects_default_subproject() RETURNS trigger AS $$ \
             BEGIN \
               INSERT INTO subprojects (project_id, name) VALUES (NEW.id, NEW.name); \
               RETURN NEW; \
             END; $$ LANGUAGE plpgsql",
        )
        .await?;
        db.execute_unprepared("DROP TRIGGER IF EXISTS projects_default_subproject_trg ON projects")
            .await?;
        db.execute_unprepared(
            "CREATE TRIGGER projects_default_subproject_trg AFTER INSERT ON projects \
             FOR EACH ROW EXECUTE FUNCTION projects_default_subproject()",
        )
        .await?;

        // Keep site.subproject_id and the denormalized site.project_id consistent, whichever the
        // caller set: an explicit subproject wins (project follows it); a project change with no
        // subproject re-derives the project's default subproject; an insert with neither is left for
        // the project's default. This makes the subproject mandatory in practice without a NOT NULL
        // column that every insert path would have to satisfy.
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
        db.execute_unprepared("DROP TRIGGER IF EXISTS sites_ensure_subproject_trg ON sites")
            .await?;
        db.execute_unprepared(
            "CREATE TRIGGER sites_ensure_subproject_trg BEFORE INSERT OR UPDATE ON sites \
             FOR EACH ROW EXECUTE FUNCTION sites_ensure_subproject()",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP TRIGGER IF EXISTS sites_ensure_subproject_trg ON sites")
            .await?;
        db.execute_unprepared("DROP FUNCTION IF EXISTS sites_ensure_subproject()")
            .await?;
        db.execute_unprepared("DROP TRIGGER IF EXISTS projects_default_subproject_trg ON projects")
            .await?;
        db.execute_unprepared("DROP FUNCTION IF EXISTS projects_default_subproject()")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS sites_subproject_idx")
            .await?;
        db.execute_unprepared("ALTER TABLE sites DROP COLUMN IF EXISTS subproject_id")
            .await?;
        db.execute_unprepared("DROP TABLE IF EXISTS subprojects")
            .await?;
        Ok(())
    }
}
