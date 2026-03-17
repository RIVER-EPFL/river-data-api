use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

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
