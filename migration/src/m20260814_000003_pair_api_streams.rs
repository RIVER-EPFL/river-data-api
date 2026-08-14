use sea_orm_migration::prelude::*;

/// Pairs the `api` streams to the slot they already write to.
///
/// A stream's `site_parameter_id` is where a reading's site and parameter come from, and every
/// other source system sets it before its readings are attributed. `/readings/batch` and the CSV
/// importer instead created their stream unpaired and stamped the attribution on each row from the
/// request, leaving rows that name a slot on a channel that names none. An overwrite arriving on
/// such a stream resolves no slot and writes that nothing over the rows.
///
/// The pairing is recoverable exactly rather than guessed: `get_or_create_api_stream` builds
/// `source_key` as `"{site_id}:{parameter_id}"`, so the two halves identify the `site_parameters`
/// row directly. A key that is not that shape, or names a slot that no longer exists, is left
/// unpaired.
///
/// Readings are untouched. This only makes the channel agree with the rows it already holds.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r"UPDATE data_streams d
                  SET site_parameter_id = sp.id,
                      paired_at = COALESCE(d.paired_at, now()),
                      updated_at = now()
                  FROM site_parameters sp
                  WHERE d.source_system = 'api'
                    AND d.site_parameter_id IS NULL
                    AND d.source_key ~ '^[0-9a-fA-F-]{36}:[0-9a-fA-F-]{36}$'
                    AND sp.site_id = split_part(d.source_key, ':', 1)::uuid
                    AND sp.parameter_id = split_part(d.source_key, ':', 2)::uuid;",
            )
            .await?;
        Ok(())
    }

    /// The pairing this restores is derivable from `source_key` at any time, so reversing it would
    /// only recreate the state that made an overwrite destructive.
    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
