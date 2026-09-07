use sea_orm_migration::prelude::*;

/// A subscription names the group of notifications it answers for.
///
/// The audience used to be the slot alone, so a person who wanted their site's alarms was enrolled
/// in sync-service alerts by the same row and could not leave one without the other. `kind_group`
/// splits the two audiences; an absent row reads as subscribed for `alarms` and unsubscribed for
/// `sync`, so the defaults hold with no row per person.
#[derive(DeriveMigrationName)]
pub struct Migration;

const UP: &str = "
    ALTER TABLE public.notification_subscriptions
        ADD COLUMN IF NOT EXISTS kind_group text NOT NULL DEFAULT 'alarms';

    ALTER TABLE public.notification_subscriptions
        DROP CONSTRAINT IF EXISTS notification_subscriptions_kind_group_check;
    ALTER TABLE public.notification_subscriptions
        ADD CONSTRAINT notification_subscriptions_kind_group_check
        CHECK (kind_group IN ('alarms', 'sync'));

    DROP INDEX IF EXISTS public.uq_notification_subscriptions_scope;
    CREATE UNIQUE INDEX uq_notification_subscriptions_scope
        ON public.notification_subscriptions (
            keycloak_sub,
            kind_group,
            COALESCE(project_id, '00000000-0000-0000-0000-000000000000'::uuid),
            COALESCE(site_id, '00000000-0000-0000-0000-000000000000'::uuid),
            COALESCE(parameter_id, '00000000-0000-0000-0000-000000000000'::uuid)
        );
";

const DOWN: &str = "
    DROP INDEX IF EXISTS public.uq_notification_subscriptions_scope;
    ALTER TABLE public.notification_subscriptions
        DROP CONSTRAINT IF EXISTS notification_subscriptions_kind_group_check;
    ALTER TABLE public.notification_subscriptions DROP COLUMN IF EXISTS kind_group;
    CREATE UNIQUE INDEX uq_notification_subscriptions_scope
        ON public.notification_subscriptions (
            keycloak_sub,
            COALESCE(project_id, '00000000-0000-0000-0000-000000000000'::uuid),
            COALESCE(site_id, '00000000-0000-0000-0000-000000000000'::uuid),
            COALESCE(parameter_id, '00000000-0000-0000-0000-000000000000'::uuid)
        );
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
