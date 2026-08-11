use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

/// Refresh continuous aggregates after new data is synced.
///
/// If `since` is provided, uses that as the start of the refresh window
/// (useful after backfills or imports). Otherwise defaults to recent windows.
pub async fn refresh_continuous_aggregates(
    db: &DatabaseConnection,
    since: Option<DateTime<Utc>>,
) {
    tracing::debug!(?since, "Refreshing continuous aggregates...");

    let hourly_start = since
        .map(|s| format!("'{}'::timestamptz", s.to_rfc3339()))
        .unwrap_or_else(|| "NOW() - INTERVAL '24 hours'".to_string());

    let daily_start = since
        .map(|s| format!("'{}'::timestamptz", s.to_rfc3339()))
        .unwrap_or_else(|| "NOW() - INTERVAL '7 days'".to_string());

    let result = db
        .execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("CALL refresh_continuous_aggregate('readings_hourly', {hourly_start}, NOW())"),
        ))
        .await;

    match &result {
        Ok(_) => tracing::debug!("Hourly continuous aggregate refreshed"),
        Err(e) => tracing::warn!(error = %e, "Failed to refresh hourly aggregate"),
    }

    let result = db
        .execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("CALL refresh_continuous_aggregate('readings_daily', {daily_start}, NOW())"),
        ))
        .await;

    match &result {
        Ok(_) => tracing::debug!("Daily continuous aggregate refreshed"),
        Err(e) => tracing::warn!(error = %e, "Failed to refresh daily aggregate"),
    }

    // Weekly and monthly need wider windows so a correction always lands inside
    // the bucket it changed; widened like the retag job's 32-day margin.
    let weekly_start = since
        .map(|s| format!("'{}'::timestamptz", (s - chrono::Duration::days(7)).to_rfc3339()))
        .unwrap_or_else(|| "NOW() - INTERVAL '14 days'".to_string());
    let monthly_start = since
        .map(|s| format!("'{}'::timestamptz", (s - chrono::Duration::days(32)).to_rfc3339()))
        .unwrap_or_else(|| "NOW() - INTERVAL '62 days'".to_string());

    for (agg, start) in [
        ("readings_weekly", weekly_start),
        ("readings_monthly", monthly_start),
    ] {
        let result = db
            .execute(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!("CALL refresh_continuous_aggregate('{agg}', {start}, NOW())"),
            ))
            .await;
        match &result {
            Ok(_) => tracing::debug!(aggregate = agg, "Continuous aggregate refreshed"),
            Err(e) => tracing::warn!(error = %e, aggregate = agg, "Failed to refresh aggregate"),
        }
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
