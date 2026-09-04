use sea_orm_migration::prelude::*;

/// The curve a source-corrected value was produced with, on the annotation that records the
/// correction. A corrected reading never carries `standard_curve_id` (raw in, curve out), so the
/// annotation is where the reference lives, machine-readable rather than only in its text. A
/// curve referenced here counts as used, so an upstream coefficient edit mints a successor
/// instead of rewriting the curve the value was made with.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "ALTER TABLE annotations
             ADD COLUMN IF NOT EXISTS standard_curve_id UUID REFERENCES standard_curves(id)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_annotations_standard_curve
             ON annotations (standard_curve_id) WHERE standard_curve_id IS NOT NULL",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_annotations_standard_curve")
            .await?;
        db.execute_unprepared("ALTER TABLE annotations DROP COLUMN IF EXISTS standard_curve_id")
            .await?;
        Ok(())
    }
}
