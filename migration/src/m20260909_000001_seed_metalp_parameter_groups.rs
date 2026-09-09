use sea_orm_migration::prelude::*;

/// Was a re-application of the parameter-group seed carrying METALP's registry; now nothing.
///
/// Kept for the same reason as `m20260908_000007`, which it re-ran: a recorded version whose file
/// is gone stops the migrator on every database that has it.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
