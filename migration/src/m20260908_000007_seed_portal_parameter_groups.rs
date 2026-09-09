use sea_orm_migration::prelude::*;

/// Was the CNET and METALP portals' categories as parameter groups; now nothing.
///
/// A database starts blank and a parameter group is something somebody at this lab agreed to
/// (Q134), so the 13 groups and 192 members this seeded are gone. The file stays because the
/// migrator refuses to run at all while `seaql_migrations` names a version whose file is missing,
/// and dev and prod have this one recorded. It goes when the chain is flattened again and the
/// ledger is rewritten with it.
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
