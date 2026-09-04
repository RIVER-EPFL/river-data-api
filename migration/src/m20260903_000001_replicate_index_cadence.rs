use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Only a spot instant has replicates. Every continuous and derived reader, and all four
/// continuous aggregates, filter `replicate_index = 0`, so a non-zero index on a non-spot row is
/// stored and served nowhere.
const CONSTRAINT: &str = "readings_replicate_index_spot_only";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        // NOT VALID: the check binds every future write without a full scan of the hypertable, and
        // the rows that predate it are reported by the audit rather than blocking the migration.
        db.execute_unprepared(&format!(
            "ALTER TABLE readings ADD CONSTRAINT {CONSTRAINT} \
             CHECK (replicate_index = 0 OR measurement_type = 'spot') NOT VALID"
        ))
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(&format!(
                "ALTER TABLE readings DROP CONSTRAINT IF EXISTS {CONSTRAINT}"
            ))
            .await?;
        Ok(())
    }
}
