use sea_orm_migration::prelude::*;

/// Give the curation a database already holds a record to have been decided by.
///
/// A database carried forward from the production tag holds rows that are flagged, withdrawn or
/// hand-curved, and nothing says who decided that or what stood before. Under the record (ADR
/// 0008) the columns are a projection, so those rows project a state no decision produced: the
/// drift sweep reports every one of them and a rollback has no prior state to restore.
///
/// One decision is synthesised per curated row from the columns themselves, actor `unknown`,
/// origin `migration`, stamped at the row's arrival when it has one. `new` carries the stored
/// value, so the projection trigger writes back exactly what it found and no number moves; `old`
/// is the uncurated state, which is what a rollback of a synthesised decision restores. A row
/// that already carries a live decision of that family is left alone, so the migration is
/// rerunnable and a database built from the baseline finds nothing to do.
#[derive(DeriveMigrationName)]
pub struct Migration;

/// The migration's SQL, exposed so the theme test runs exactly what the migrator runs. The
/// decompression cap is lifted because the projection trigger updates the rows it reads, and a
/// curated row may sit in a compressed chunk.
#[must_use]
pub fn synthesise_curation_record() -> String {
    let no_live = |family: &str| {
        format!(
            "NOT EXISTS (SELECT 1 FROM reading_decisions d
                          WHERE d.stream_id = r.stream_id AND d.time = r.time
                            AND (d.replicate_index IS NULL
                                 OR d.replicate_index = r.replicate_index)
                            AND d.kind IN ({family}) AND d.rolled_back_by IS NULL)"
        )
    };
    format!(
        "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0;

         INSERT INTO reading_decisions
             (stream_id, time, replicate_index, kind, old, new, actor, at, reason, origin)
         SELECT r.stream_id, r.time, r.replicate_index, 'flag',
                jsonb_build_object('is_flagged', false, 'flag_reason', NULL),
                jsonb_build_object('reason', r.flag_reason),
                'unknown', COALESCE(r.ingested_at, now()),
                'synthesised from the stored flag when the record was introduced', 'migration'
           FROM readings r
          WHERE r.is_flagged IS TRUE AND {no_flag};

         INSERT INTO reading_decisions
             (stream_id, time, replicate_index, kind, old, new, actor, at, reason, origin)
         SELECT r.stream_id, r.time, r.replicate_index, 'withdraw',
                jsonb_build_object('withdrawn_at', NULL, 'withdrawn_reason', NULL),
                jsonb_build_object('withdrawn_at', r.withdrawn_at, 'reason', r.withdrawn_reason),
                'unknown', COALESCE(r.ingested_at, now()),
                'synthesised from the stored retraction when the record was introduced',
                'migration'
           FROM readings r
          WHERE r.withdrawn_at IS NOT NULL AND {no_withdrawn};

         INSERT INTO reading_decisions
             (stream_id, time, replicate_index, kind, old, new, actor, at, reason, origin)
         SELECT r.stream_id, r.time, r.replicate_index, 'curve',
                jsonb_build_object('standard_curve_id', NULL),
                jsonb_build_object('standard_curve_id', r.standard_curve_id),
                'unknown', COALESCE(r.ingested_at, now()),
                'synthesised from the stored curve when the record was introduced', 'migration'
           FROM readings r
          WHERE r.standard_curve_id IS NOT NULL AND {no_curve};",
        no_flag = no_live("'flag', 'unflag'"),
        no_withdrawn = no_live("'withdraw', 'reassert', 'reject'"),
        no_curve = no_live("'curve'"),
    )
}

/// Only the synthesised rows go: a decision anyone actually took stays.
const DROP_SYNTHESISED: &str = "
    DELETE FROM reading_decisions WHERE origin = 'migration' AND actor = 'unknown';
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(&synthesise_curation_record())
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(DROP_SYNTHESISED)
            .await?;
        Ok(())
    }
}
