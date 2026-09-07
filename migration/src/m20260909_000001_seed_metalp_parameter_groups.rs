use sea_orm_migration::prelude::*;

/// Re-applies the parameter-group seed, which now carries METALP's registry as well as CNET's.
///
/// `m20260908_000007` has already run on databases that hold only the nine CNET categories, and a
/// migration is never re-run, so the four METALP-only groups and the columns it alone lists would
/// never reach them. The seed is idempotent by construction (a parameter whose code exists keeps
/// its row, a group or membership already there is left alone), and METALP's columns are appended
/// after CNET's inside a shared category, so nothing an operator ordered or edited moves.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(crate::m20260908_000007_seed_portal_parameter_groups::SEED)
            .await?;
        Ok(())
    }

    /// The seed's own `down` removes every group and membership, so there is nothing this one
    /// could undo that reverting `m20260908_000007` does not.
    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
