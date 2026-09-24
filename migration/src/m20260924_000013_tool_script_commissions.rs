use sea_orm_migration::prelude::*;

/// Each decommission and recommission of a calculation, under the name it held before the event.
/// A decommission frees the name for a new calculation by suffixing it, so the one it held is kept
/// here for a recommission to restore. The three stamp columns on `tool_scripts` stay the current
/// state; this is the history behind them.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
CREATE TABLE public.tool_script_commissions (
    id uuid DEFAULT gen_random_uuid() NOT NULL PRIMARY KEY,
    tool_script_id uuid NOT NULL REFERENCES public.tool_scripts(id) ON DELETE CASCADE,
    event text NOT NULL CHECK (event IN ('decommissioned', 'recommissioned')),
    name text NOT NULL,
    actor text NOT NULL,
    at timestamptz DEFAULT now() NOT NULL,
    reason text NOT NULL
);
CREATE INDEX idx_tool_script_commissions_script
    ON public.tool_script_commissions (tool_script_id, at);
INSERT INTO public.tool_script_commissions (tool_script_id, event, name, actor, at, reason)
SELECT id, 'decommissioned', name, decommissioned_by, decommissioned_at, decommission_reason
FROM public.tool_scripts
WHERE decommissioned_at IS NOT NULL;
UPDATE public.tool_scripts
SET name = name || '_decommissioned_' || to_char(decommissioned_at AT TIME ZONE 'UTC', 'YYYYMMDD')
WHERE decommissioned_at IS NOT NULL;
";

const DOWN: &str = r"
UPDATE public.tool_scripts s
SET name = c.name
FROM (
    SELECT DISTINCT ON (tool_script_id) tool_script_id, name
    FROM public.tool_script_commissions
    WHERE event = 'decommissioned'
    ORDER BY tool_script_id, at DESC
) c
WHERE s.id = c.tool_script_id AND s.decommissioned_at IS NOT NULL;
DROP TABLE IF EXISTS public.tool_script_commissions;
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
