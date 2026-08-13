use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"ALTER TABLE site_parameters ADD COLUMN IF NOT EXISTS is_public BOOLEAN NOT NULL DEFAULT FALSE"#,
        )
        .await?;

        // Backfill: mark site_parameters as public if they were in public_exposed_parameters
        db.execute_unprepared(
            r#"UPDATE site_parameters sp SET is_public = true
               WHERE EXISTS (
                   SELECT 1 FROM public_exposed_parameters pep
                   JOIN sites s ON s.id = sp.site_id
                   WHERE pep.parameter_id = sp.parameter_id
                     AND pep.project_id = s.project_id
               )"#,
        )
        .await?;

        db.execute_unprepared(r#"DROP TABLE IF EXISTS public_exposed_parameters"#)
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS public_exposed_parameters (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                project_id UUID NOT NULL REFERENCES projects(id),
                parameter_id UUID NOT NULL REFERENCES parameters(id),
                public_name VARCHAR(128) NOT NULL,
                public_units VARCHAR(32) NOT NULL DEFAULT '',
                description TEXT,
                sort_order INTEGER NOT NULL DEFAULT 0,
                conversion_factor DOUBLE PRECISION NOT NULL DEFAULT 1.0,
                conversion_offset DOUBLE PRECISION NOT NULL DEFAULT 0.0,
                include_derived BOOLEAN NOT NULL DEFAULT FALSE,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )"#,
        )
        .await?;

        db.execute_unprepared(r#"ALTER TABLE site_parameters DROP COLUMN IF EXISTS is_public"#)
            .await?;

        Ok(())
    }
}
