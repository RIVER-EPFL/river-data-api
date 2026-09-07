use sea_orm_migration::prelude::*;

/// `fitted_on` on `standard_curves`: the date the curve was fitted, which is what a curve is
/// identified by in the lab. A synced curve folds it into `name`; a hand-entered one had only
/// `created_at`, the row's own arrival. Backfilled from `created_at` so every existing curve
/// carries a date.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = r"
    ALTER TABLE standard_curves ADD COLUMN IF NOT EXISTS fitted_on DATE;
    UPDATE standard_curves SET fitted_on = created_at::date WHERE fitted_on IS NULL;
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(UP).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE standard_curves DROP COLUMN IF EXISTS fitted_on;")
            .await?;
        Ok(())
    }
}
