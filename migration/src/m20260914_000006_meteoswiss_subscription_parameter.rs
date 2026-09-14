use sea_orm_migration::prelude::*;

/// A MeteoSwiss subscription names the catalog parameter it lands on.
///
/// The row is minted with the subscription, so a subscription pointing at nothing is a state the
/// column no longer allows rather than a warning every tick repeats.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        // The pressure row, for a subscription that predates the minting and has none.
        db.execute_unprepared(
            "INSERT INTO public.parameters (code, name, default_units, category, description)
             SELECT 'barometric_pressure', 'Barometric Pressure', 'hPa', 'measurement',
                    'MeteoSwiss SMN prestas0, landed per subscribed station'
              WHERE EXISTS (SELECT 1 FROM public.meteoswiss_subscriptions
                             WHERE parameter_id IS NULL AND lower(variable) = 'prestas0')
                AND NOT EXISTS (SELECT 1 FROM public.parameters
                                 WHERE lower(code) = 'barometric_pressure')",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE public.meteoswiss_subscriptions sub
                SET parameter_id = p.id
               FROM public.parameters p
              WHERE sub.parameter_id IS NULL
                AND lower(sub.variable) = 'prestas0'
                AND lower(p.code) = 'barometric_pressure'",
        )
        .await?;
        db.execute_unprepared(
            "ALTER TABLE public.meteoswiss_subscriptions
                 ALTER COLUMN parameter_id SET NOT NULL",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE public.meteoswiss_subscriptions
                     ALTER COLUMN parameter_id DROP NOT NULL",
            )
            .await?;
        Ok(())
    }
}
