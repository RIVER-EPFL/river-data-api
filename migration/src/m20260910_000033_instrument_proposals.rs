use sea_orm_migration::prelude::*;

/// A source's own instrument register, held until a pairing plan admits it.
///
/// Q134 decided an instrument exists once a plan an operator validated creates it, so a sync no
/// longer mints one. The register still has to arrive: 125 METALP rows carry the serial, the model,
/// the station and the installation date, 34 of them have no serial at all and one serial is on two
/// probes, so the row is the identity and re-typing it by hand is not a repair. It waits here.
#[derive(DeriveMigrationName)]
pub struct Migration;

pub const UP: &str = r"
    CREATE TABLE IF NOT EXISTS public.instrument_proposals (
        id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
        source_system  character varying(64) NOT NULL,
        source_key     character varying(255) NOT NULL,
        name           text NOT NULL,
        serial_number  text,
        manufacturer   text,
        model          text,
        notes          text,
        is_lab_instrument boolean NOT NULL DEFAULT false,
        data_frequency character varying(16),
        metadata       jsonb,
        first_seen_at  timestamptz NOT NULL DEFAULT now(),
        last_seen_at   timestamptz NOT NULL DEFAULT now(),
        CONSTRAINT instrument_proposals_provenance UNIQUE (source_system, source_key)
    );

    ALTER TABLE public.pairing_plans
        ADD COLUMN IF NOT EXISTS instrument_proposals jsonb NOT NULL DEFAULT '[]'::jsonb;
";

pub const DOWN: &str = r"
    ALTER TABLE public.pairing_plans DROP COLUMN IF EXISTS instrument_proposals;
    DROP TABLE IF EXISTS public.instrument_proposals;
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
