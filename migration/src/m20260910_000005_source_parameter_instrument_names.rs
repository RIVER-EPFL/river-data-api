use sea_orm_migration::prelude::*;

/// Name a source-parameter or lab instrument for its parameter and its source, not for a station.
///
/// One such row serves every station that reports the parameter, so a "{site} {parameter}" name is
/// true of whichever station registered first and of no other. `(source_system, source_key)` is
/// untouched: identity, pairing and attribution do not move, only the label an operator reads.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = "
    UPDATE public.sensors
       SET name = substring(source_key from length(source_system) + 2) || ' (' || source_system || ')'
     WHERE kind IN ('source_parameter', 'lab')
       AND source_system IS NOT NULL
       AND source_key LIKE source_system || ':%'
       AND name IS DISTINCT FROM
           substring(source_key from length(source_system) + 2) || ' (' || source_system || ')';
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(UP).await?;
        Ok(())
    }

    /// The station-prefixed names cannot be recovered, and nothing reads them.
    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
