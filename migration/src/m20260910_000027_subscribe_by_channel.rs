use sea_orm_migration::prelude::*;

/// A notification is subscribed per kind, not per group of kinds (Q57, M163).
///
/// `kind_group` becomes `channel`, and each stored group row is expanded into one row per kind the
/// group covered, with the same scope and the same answer. Nobody's audience changes: a subscriber
/// who had turned the sync group on is subscribed to each of its kinds, and one who never opened
/// the page still has no rows and so still takes each channel's own default.
///
/// The vocabulary CHECK goes with the group: the channels are the kinds the triggers emit, which
/// grow with the triggers, and a CHECK over them would need a migration per new kind. The write
/// path refuses an unknown channel by name (`notifications::me::set_my_subscriptions`), and a row
/// naming a kind nothing emits reaches nobody rather than corrupting anything.
#[derive(DeriveMigrationName)]
pub struct Migration;

/// The kinds each group covered, as the group's own code listed them.
const GROUPS: [(&str, &[&str]); 3] = [
    ("alarms", &["alarm_opened", "alarm_resolved"]),
    (
        "sync",
        &[
            "stale_data",
            "sync_stale",
            "sync_failure",
            "streams_unpaired",
            "holds_open",
            "job_failed",
            "changes_pending",
            "curve_drift",
            "derived_computed",
        ],
    ),
    ("prediction", &["battery_forecast"]),
];

fn expansions() -> String {
    GROUPS
        .iter()
        .flat_map(|(group, kinds)| {
            kinds.iter().map(move |kind| {
                format!(
                    "INSERT INTO notification_subscriptions \
                         (keycloak_sub, channel, project_id, site_id, parameter_id, enabled) \
                     SELECT keycloak_sub, '{kind}', project_id, site_id, parameter_id, enabled \
                       FROM notification_subscriptions WHERE channel = '{group}';"
                )
            })
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn up() -> String {
    format!(
        "ALTER TABLE public.notification_subscriptions \
             DROP CONSTRAINT IF EXISTS notification_subscriptions_kind_group_check;\n\
         ALTER TABLE public.notification_subscriptions RENAME COLUMN kind_group TO channel;\n{}\n\
         DELETE FROM public.notification_subscriptions WHERE channel IN ('alarms', 'sync', 'prediction');",
        expansions()
    )
}

/// Back to the group, keeping one row per group and scope: the expansion is many-to-one, so the
/// rows that disagreed within a group collapse onto whichever the group's own name takes.
pub const DOWN: &str = r"
    DELETE FROM public.notification_subscriptions
     WHERE channel NOT IN ('alarms', 'sync', 'prediction');
    ALTER TABLE public.notification_subscriptions RENAME COLUMN channel TO kind_group;
    ALTER TABLE public.notification_subscriptions
        ADD CONSTRAINT notification_subscriptions_kind_group_check
        CHECK (kind_group IN ('alarms', 'sync', 'prediction'));
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(&up()).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(DOWN).await?;
        Ok(())
    }
}
