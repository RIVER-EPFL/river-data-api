use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    // standard_curves is folded into the unified sensor_calibrations model (mode = 'instant' curves
    // carry the same slope/intercept/parameter/r_squared). The table is empty in every deployment, so
    // drop it. down() recreates the shape (no data) for local reversibility.
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS standard_curves CASCADE;")
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
                CREATE TABLE IF NOT EXISTS standard_curves (
                    id UUID PRIMARY KEY,
                    parameter_id UUID NOT NULL REFERENCES parameters(id),
                    name TEXT,
                    slope DOUBLE PRECISION NOT NULL,
                    intercept DOUBLE PRECISION NOT NULL,
                    r_squared DOUBLE PRECISION,
                    valid_from TIMESTAMPTZ,
                    notes TEXT,
                    created_at TIMESTAMPTZ DEFAULT now()
                );
                CREATE INDEX IF NOT EXISTS idx_standard_curves_parameter ON standard_curves (parameter_id);
                "#,
            )
            .await?;
        Ok(())
    }
}
