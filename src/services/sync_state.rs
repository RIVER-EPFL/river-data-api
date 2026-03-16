use chrono::{Duration, Utc};
use sea_orm::{ActiveModelTrait, ConnectionTrait, DatabaseConnection, EntityTrait, Set, Statement};
use uuid::Uuid;

use crate::entity::sync_state;

pub async fn update_sync_state_success(
    db: &DatabaseConnection,
    site_parameter_id: Uuid,
    latest_time: chrono::DateTime<Utc>,
) {
    let state = sync_state::ActiveModel {
        site_parameter_id: Set(site_parameter_id),
        last_data_time: Set(Some(latest_time.into())),
        last_sync_attempt: Set(Some(Utc::now().into())),
        sync_status: Set(Some("success".to_string())),
        error_message: Set(None),
        retry_count: Set(Some(0)),
        last_full_sync: sea_orm::ActiveValue::NotSet,
    };

    if let Err(e) = sync_state::Entity::insert(state)
        .on_conflict(
            sea_orm::sea_query::OnConflict::column(sync_state::Column::SiteParameterId)
                .update_columns([
                    sync_state::Column::LastDataTime,
                    sync_state::Column::LastSyncAttempt,
                    sync_state::Column::SyncStatus,
                    sync_state::Column::ErrorMessage,
                    sync_state::Column::RetryCount,
                ])
                .to_owned(),
        )
        .exec(db)
        .await
    {
        tracing::warn!(
            site_parameter_id = %site_parameter_id,
            error = %e,
            "Failed to update sync state"
        );
    }
}

pub async fn update_sync_state_error(
    db: &DatabaseConnection,
    site_parameter_id: Uuid,
    error: &str,
) {
    let current = sync_state::Entity::find_by_id(site_parameter_id)
        .one(db)
        .await
        .ok()
        .flatten();

    let retry_count = current.and_then(|s| s.retry_count).unwrap_or(0) + 1;

    let state = sync_state::ActiveModel {
        site_parameter_id: Set(site_parameter_id),
        last_data_time: Set(None),
        last_sync_attempt: Set(Some(Utc::now().into())),
        sync_status: Set(Some("error".to_string())),
        error_message: Set(Some(error.to_string())),
        retry_count: Set(Some(retry_count)),
        last_full_sync: sea_orm::ActiveValue::NotSet,
    };

    if let Err(e) = sync_state::Entity::insert(state)
        .on_conflict(
            sea_orm::sea_query::OnConflict::column(sync_state::Column::SiteParameterId)
                .update_columns([
                    sync_state::Column::LastSyncAttempt,
                    sync_state::Column::SyncStatus,
                    sync_state::Column::ErrorMessage,
                    sync_state::Column::RetryCount,
                ])
                .to_owned(),
        )
        .exec(db)
        .await
    {
        tracing::warn!(
            site_parameter_id = %site_parameter_id,
            error = %e,
            "Failed to update sync state error"
        );
    }
}

/// Check if a full re-sync is needed (oldest `last_full_sync` > 24 hours ago, or never done).
pub async fn needs_full_sync(db: &DatabaseConnection) -> bool {
    let states = match sync_state::Entity::find().all(db).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to check full sync status, assuming needed");
            return true;
        }
    };

    if states.is_empty() {
        return true;
    }

    let now = Utc::now();
    let threshold = Duration::hours(24);

    for state in states {
        match state.last_full_sync {
            None => return true,
            Some(last) => {
                let last_utc = last.with_timezone(&Utc);
                if now - last_utc > threshold {
                    return true;
                }
            }
        }
    }

    false
}

/// Update `last_full_sync` timestamp for all `site_parameters`.
pub async fn update_last_full_sync_for_all_parameters(db: &DatabaseConnection) {
    let now = Utc::now();

    let states = match sync_state::Entity::find().all(db).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to fetch sync states for full sync update");
            return;
        }
    };

    for state in states {
        let mut active: sync_state::ActiveModel = state.into();
        active.last_full_sync = Set(Some(now.into()));

        if let Err(e) = active.update(db).await {
            tracing::warn!(error = %e, "Failed to update last_full_sync");
        }
    }
}

/// Refresh continuous aggregates after new data is synced.
pub async fn refresh_continuous_aggregates(db: &DatabaseConnection) {
    tracing::debug!("Refreshing continuous aggregates...");

    let result = db
        .execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "CALL refresh_continuous_aggregate('readings_hourly', NOW() - INTERVAL '24 hours', NOW())".to_string(),
        ))
        .await;

    match result {
        Ok(_) => tracing::debug!("Hourly continuous aggregate refreshed"),
        Err(e) => tracing::warn!(error = %e, "Failed to refresh hourly aggregate"),
    }

    let result = db
        .execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "CALL refresh_continuous_aggregate('readings_daily', NOW() - INTERVAL '7 days', NOW())"
                .to_string(),
        ))
        .await;

    match result {
        Ok(_) => tracing::debug!("Daily continuous aggregate refreshed"),
        Err(e) => tracing::warn!(error = %e, "Failed to refresh daily aggregate"),
    }
}

/// Refresh all continuous aggregates for the entire data range.
pub async fn refresh_continuous_aggregates_full(db: &DatabaseConnection) {
    tracing::info!("Refreshing continuous aggregates for full history...");

    let aggregates = [
        "readings_hourly",
        "readings_daily",
        "readings_weekly",
        "readings_monthly",
    ];

    for agg in aggregates {
        let result = db
            .execute(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!("CALL refresh_continuous_aggregate('{agg}', NULL, NULL)"),
            ))
            .await;

        match result {
            Ok(_) => tracing::info!(aggregate = agg, "Continuous aggregate refreshed"),
            Err(e) => tracing::warn!(error = %e, aggregate = agg, "Failed to refresh aggregate"),
        }
    }

    tracing::info!("Full continuous aggregate refresh completed");
}
