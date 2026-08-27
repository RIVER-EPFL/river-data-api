use sea_orm_migration::prelude::*;

/// `tool_runs`: one row per successful `/tools/{name}/calculate`, written by the server at
/// calculate time. A grab save that came from a tool names the run (`tool_run_id`) and the server
/// builds the provenance blob from this row, so the blob's inputs, constants, curves and outputs
/// are what the engine actually resolved and produced. A client can no longer author the blob:
/// linking a tool a save did not run has nothing to reference, and every claim in the blob is a
/// column here, written before the save existed.
///
/// `curves` doubles as the no-double-correction guard: a run that consumed a standard curve
/// produced corrected outputs, so a save referencing it is refused a `standard_curve_id`
/// (ADR 0003: a stored curve id means raw in, curve out).
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS tool_runs (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                tool_name TEXT NOT NULL,
                tool_version JSONB NOT NULL,
                inputs JSONB NOT NULL,
                constants JSONB NOT NULL,
                curves JSONB NOT NULL,
                outputs JSONB NOT NULL,
                created_by TEXT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
            );",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_tool_runs_created_at ON tool_runs (created_at);",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP TABLE IF EXISTS tool_runs;")
            .await?;
        Ok(())
    }
}
