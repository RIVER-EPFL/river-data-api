use sea_orm_migration::prelude::*;

/// A third notification audience: `prediction`.
///
/// The battery-low forecast was delivered under `alarms`, which is the one group a subscriber is in
/// without a row, so an instrument status warning reached everyone who wanted their site's data
/// alarms. It is a maintenance advisory rather than a measurement out of range, so it becomes its
/// own opt-in group (Q81).
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = "
    ALTER TABLE public.notification_subscriptions
        DROP CONSTRAINT IF EXISTS notification_subscriptions_kind_group_check;
    ALTER TABLE public.notification_subscriptions
        ADD CONSTRAINT notification_subscriptions_kind_group_check
        CHECK (kind_group IN ('alarms', 'sync', 'prediction'));
";

const DOWN: &str = "
    DELETE FROM public.notification_subscriptions WHERE kind_group = 'prediction';
    ALTER TABLE public.notification_subscriptions
        DROP CONSTRAINT IF EXISTS notification_subscriptions_kind_group_check;
    ALTER TABLE public.notification_subscriptions
        ADD CONSTRAINT notification_subscriptions_kind_group_check
        CHECK (kind_group IN ('alarms', 'sync'));
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
