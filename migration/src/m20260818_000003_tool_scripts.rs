use sea_orm_migration::prelude::*;

/// Portal-authored analytical tool scripts, executed by the R runner. A tool's code is a chain
/// of immutable versions; `active_version_id` is the only mutable pointer, and every flip of it
/// is recorded, so rollback is activating an older version and history never rewrites.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r"
            CREATE TABLE tool_scripts (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                name TEXT NOT NULL,
                label TEXT NOT NULL,
                description TEXT,
                active_version_id UUID,
                created_by TEXT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
            );
            CREATE UNIQUE INDEX idx_tool_scripts_name ON tool_scripts (LOWER(name));
            ",
        )
        .await?;

        db.execute_unprepared(
            r"
            CREATE TABLE tool_script_versions (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                tool_script_id UUID NOT NULL REFERENCES tool_scripts(id) ON DELETE CASCADE,
                version_no INT NOT NULL,
                script TEXT NOT NULL,
                entry_function TEXT NOT NULL DEFAULT 'tool',
                manifest JSONB NOT NULL,
                test_cases JSONB NOT NULL DEFAULT '{}'::jsonb,
                content_hash TEXT NOT NULL,
                created_by TEXT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                validated_at TIMESTAMPTZ,
                UNIQUE (tool_script_id, version_no),
                UNIQUE (tool_script_id, content_hash)
            );
            CREATE INDEX idx_tool_script_versions_script
                ON tool_script_versions (tool_script_id, version_no DESC);
            ",
        )
        .await?;

        db.execute_unprepared(
            r"
            ALTER TABLE tool_scripts
                ADD CONSTRAINT fk_tool_scripts_active_version
                FOREIGN KEY (active_version_id) REFERENCES tool_script_versions(id);
            ",
        )
        .await?;

        db.execute_unprepared(
            r"
            CREATE TABLE tool_script_activations (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                tool_script_id UUID NOT NULL REFERENCES tool_scripts(id) ON DELETE CASCADE,
                from_version_id UUID REFERENCES tool_script_versions(id),
                to_version_id UUID NOT NULL REFERENCES tool_script_versions(id),
                activated_by TEXT,
                activated_at TIMESTAMPTZ NOT NULL DEFAULT now()
            );
            CREATE INDEX idx_tool_script_activations_script
                ON tool_script_activations (tool_script_id, activated_at DESC);
            ",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r"
                DROP TABLE IF EXISTS tool_script_activations;
                ALTER TABLE tool_scripts DROP CONSTRAINT IF EXISTS fk_tool_scripts_active_version;
                DROP TABLE IF EXISTS tool_script_versions;
                DROP TABLE IF EXISTS tool_scripts;
                ",
            )
            .await?;
        Ok(())
    }
}
