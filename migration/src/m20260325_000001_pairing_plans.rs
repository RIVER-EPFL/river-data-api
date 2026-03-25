use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Create pairing_plans table
        manager
            .get_connection()
            .execute_unprepared(
                r#"
                CREATE TABLE IF NOT EXISTS pairing_plans (
                    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                    source_system TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'draft',
                    created_by TEXT,
                    summary JSONB NOT NULL DEFAULT '{}',
                    entries JSONB NOT NULL DEFAULT '[]',
                    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                    applied_at TIMESTAMPTZ,
                    apply_result JSONB
                );

                CREATE INDEX IF NOT EXISTS idx_pairing_plans_source_system
                    ON pairing_plans (source_system);
                CREATE INDEX IF NOT EXISTS idx_pairing_plans_status
                    ON pairing_plans (status);

                -- Track which plan paired each stream
                ALTER TABLE data_streams
                    ADD COLUMN IF NOT EXISTS pairing_plan_id UUID REFERENCES pairing_plans(id);
                "#,
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
                ALTER TABLE data_streams DROP COLUMN IF EXISTS pairing_plan_id;
                DROP TABLE IF EXISTS pairing_plans;
                "#,
            )
            .await?;

        Ok(())
    }
}
