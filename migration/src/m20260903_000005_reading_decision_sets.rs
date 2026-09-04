use sea_orm_migration::prelude::*;

/// A decision made over a selection (a pin over a stream, a slot and window, or explicit keys)
/// is recorded once here and materialised as one `reading_decisions` row per reading, so the
/// projection stays per row while the set stays inspectable and reversible as one (ADR 0008).
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r"
                CREATE TABLE IF NOT EXISTS reading_decision_sets (
                    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                    kind TEXT NOT NULL,
                    selection JSONB NOT NULL,
                    new JSONB NOT NULL DEFAULT '{}'::jsonb,
                    actor TEXT NOT NULL,
                    at TIMESTAMPTZ NOT NULL DEFAULT now(),
                    reason TEXT,
                    rows_decided BIGINT NOT NULL DEFAULT 0,
                    rolled_back_at TIMESTAMPTZ,
                    rolled_back_by TEXT
                );
                ",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS reading_decision_sets")
            .await?;
        Ok(())
    }
}
