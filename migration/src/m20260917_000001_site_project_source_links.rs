use sea_orm_migration::prelude::*;

/// The source names a site and a project answer to (M277), so renaming either keeps every later
/// stream and note its sources send. One row per source name, since one site is fed by several.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
CREATE TABLE public.site_source_links (
    source_system character varying(64) NOT NULL,
    source_key text NOT NULL,
    site_id uuid NOT NULL REFERENCES public.sites(id) ON DELETE CASCADE,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    PRIMARY KEY (source_system, source_key)
);
CREATE INDEX idx_site_source_links_site ON public.site_source_links (site_id);

CREATE TABLE public.project_source_links (
    source_system character varying(64) NOT NULL,
    source_key text NOT NULL,
    project_id uuid NOT NULL REFERENCES public.projects(id) ON DELETE CASCADE,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    PRIMARY KEY (source_system, source_key)
);
CREATE INDEX idx_project_source_links_project ON public.project_source_links (project_id);
";

const DOWN: &str = r"
DROP TABLE public.project_source_links;
DROP TABLE public.site_source_links;
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(UP).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(DOWN).await?;
        Ok(())
    }
}
