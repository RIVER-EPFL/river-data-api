use sea_orm_migration::prelude::*;

/// The objects a plan's review has accepted, recorded rather than inferred.
///
/// A project, site or parameter the plan creates is one decision however many rows name it. That
/// decision was read back off the rows that carried it, which no row can express until the
/// decision is already taken, so it could never be taken at all. It lives on the plan from here.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = r"
    ALTER TABLE public.pairing_plans
        ADD COLUMN IF NOT EXISTS accepted_objects jsonb NOT NULL DEFAULT '[]'::jsonb;
";

pub const DOWN: &str = r"
    ALTER TABLE public.pairing_plans DROP COLUMN IF EXISTS accepted_objects;
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
