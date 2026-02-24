use axum::{extract::{Path, State}, Json};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::AppResult;

pub async fn recompute_derived(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    let db = state.db.clone();
    tokio::spawn(async move {
        tracing::info!(derived_id = %id, "Recomputing derived parameter");
        // Query all readings timestamps for this derived parameter and recompute each
        use sea_orm::{ConnectionTrait, Statement};
        let timestamps = db
            .query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT DISTINCT r.time
                  FROM readings r
                  JOIN parameters p ON r.parameter_id = p.id
                  WHERE p.derived_definition_id = $1
                  ORDER BY r.time",
                [id.into()],
            ))
            .await;

        match timestamps {
            Ok(rows) => {
                // Get the site_id for this derived parameter
                let site_row = db
                    .query_one(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        r"SELECT DISTINCT p.site_id FROM parameters p WHERE p.derived_definition_id = $1 LIMIT 1",
                        [id.into()],
                    ))
                    .await;

                if let Ok(Some(site)) = site_row {
                    let site_id: Uuid = match site.try_get("", "site_id") {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to get site_id");
                            return;
                        }
                    };

                    let total = rows.len();
                    for (i, row) in rows.iter().enumerate() {
                        let time: chrono::DateTime<chrono::FixedOffset> = match row.try_get("", "time") {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let utc_time = time.with_timezone(&chrono::Utc);
                        if let Err(e) = crate::services::calibration::recalculate_derived_at_timestamp(&db, site_id, utc_time).await {
                            tracing::error!(error = %e, time = %time, "Failed to recompute derived value");
                        }
                        if (i + 1) % 1000 == 0 {
                            tracing::info!("Recomputed {}/{} timestamps", i + 1, total);
                        }
                    }
                    tracing::info!(derived_id = %id, total = total, "Derived parameter recomputation complete");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to query timestamps for recomputation");
            }
        }
    });
    Ok(Json(serde_json::json!({ "status": "triggered" })))
}
