use sea_orm_migration::prelude::*;

/// Tightens autovacuum on `reprocessing_jobs`, the highest-churn table in the schema.
///
/// Every tracked job is UPDATEd several times over its life (claim, lease heartbeat, progress,
/// terminal) and retention holds the row count at its `JOB_MAINTENANCE_MAX_ROWS` cap, so the
/// server-default scale factor of 0.2 lets ~10k dead tuples accumulate between vacuums. The
/// partial indexes the worker claims through (`idx_reprocessing_jobs_claimable`,
/// `idx_reprocessing_jobs_lease`) carry those dead pointers, and the claim runs every
/// `POLL_SECONDS` on every replica: on dev its bitmap scans returned 2,654 and 3,417 index entries
/// for zero live rows, rechecked against 952 heap pages.
///
/// Per-table, because this table's write pattern is nothing like the hypertables' and the server
/// setting applies to both. `autovacuum_naptime` has no per-table form, so the cadence floor stays
/// wherever the server has it.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE reprocessing_jobs SET ( \
                     autovacuum_vacuum_scale_factor = 0.02, \
                     autovacuum_vacuum_threshold = 200, \
                     autovacuum_vacuum_cost_limit = 2000, \
                     autovacuum_analyze_scale_factor = 0.05 \
                 );",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE reprocessing_jobs RESET ( \
                     autovacuum_vacuum_scale_factor, \
                     autovacuum_vacuum_threshold, \
                     autovacuum_vacuum_cost_limit, \
                     autovacuum_analyze_scale_factor \
                 );",
            )
            .await?;
        Ok(())
    }
}
