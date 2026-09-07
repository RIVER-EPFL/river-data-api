use sea_orm_migration::prelude::*;

/// Say what an instrument row is, on the row.
///
/// Three of the four minting paths produce something that is not a device: the source's instrument
/// for one parameter across every station, the per-slot entry channel behind a hand-typed value,
/// and the pseudo-instrument a portal curve label mints. `is_lab_instrument` is true for all three
/// and for real lab devices, so nothing distinguished them. `(source_system, source_key)` remains
/// the identity; this is what a picker and the inventory read.
///
/// The backfill applies the minting rules to the rows that predate the column: the entry channels
/// by their internal source systems, the source-parameter rows by the `minted_from_stream` note
/// `resolve_or_mint_stream_instrument` writes, whatever lab rows remain by `is_lab_instrument`.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = "
    ALTER TABLE public.sensors ADD COLUMN IF NOT EXISTS kind text NOT NULL DEFAULT 'device';

    UPDATE public.sensors SET kind = 'entry_channel'
     WHERE source_system IN ('grab_sample', 'api');

    UPDATE public.sensors SET kind = 'source_parameter'
     WHERE kind = 'device' AND metadata ? 'minted_from_stream';

    UPDATE public.sensors SET kind = 'lab'
     WHERE kind = 'device' AND is_lab_instrument IS TRUE;

    ALTER TABLE public.sensors DROP CONSTRAINT IF EXISTS sensors_kind_check;
    ALTER TABLE public.sensors ADD CONSTRAINT sensors_kind_check
        CHECK (kind IN ('device', 'lab', 'source_parameter', 'entry_channel'));
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(UP).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.sensors DROP CONSTRAINT IF EXISTS sensors_kind_check;
                 ALTER TABLE public.sensors DROP COLUMN IF EXISTS kind;",
            )
            .await?;
        Ok(())
    }
}
