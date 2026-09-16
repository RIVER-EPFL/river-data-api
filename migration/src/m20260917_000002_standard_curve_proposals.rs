use sea_orm_migration::prelude::*;

/// Where a source's standard curves wait for a pairing plan.
///
/// A curve names the instrument it was fitted on (`standard_curves.sensor_id` is NOT NULL), and the
/// portal names none: its label is the curve's parameter cell (Q195). So a curve a sync replicates
/// before a plan has attached it to one of its instruments is held here, the plan records which
/// instrument the review attached it to, and the apply creates it.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS public.standard_curve_proposals (
                     id uuid DEFAULT gen_random_uuid() NOT NULL,
                     source_system character varying(64) NOT NULL,
                     source_key character varying(255) NOT NULL,
                     label text NOT NULL,
                     name text,
                     slope double precision NOT NULL,
                     intercept double precision NOT NULL,
                     r_squared double precision,
                     fitted_on date,
                     notes text,
                     first_seen_at timestamp with time zone DEFAULT now() NOT NULL,
                     last_seen_at timestamp with time zone DEFAULT now() NOT NULL,
                     CONSTRAINT standard_curve_proposals_pkey PRIMARY KEY (id),
                     CONSTRAINT standard_curve_proposals_provenance
                         UNIQUE (source_system, source_key)
                 );

                 ALTER TABLE public.pairing_plans
                     ADD COLUMN IF NOT EXISTS curve_attachments jsonb DEFAULT '[]'::jsonb NOT NULL",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.pairing_plans DROP COLUMN IF EXISTS curve_attachments;
                 DROP TABLE IF EXISTS public.standard_curve_proposals",
            )
            .await?;
        Ok(())
    }
}
