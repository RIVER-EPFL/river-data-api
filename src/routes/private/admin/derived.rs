use axum::{
    Json,
    extract::{Path, State},
};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::AppResult;

pub async fn recompute_derived(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    let db = state.db.clone();
    tokio::spawn(async move {
        match tokio::time::timeout(std::time::Duration::from_secs(600), async {
            tracing::info!(derived_id = %id, "Recomputing derived parameter");
            use sea_orm::{ConnectionTrait, Statement};
            let timestamps = db
                .query_all(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    r"SELECT DISTINCT r.site_id, r.time
                      FROM readings r
                      JOIN site_parameters sp
                        ON sp.site_id = r.site_id
                       AND sp.is_derived = true
                       AND sp.derived_definition_id = $1
                      JOIN derived_parameter_sources dps
                        ON dps.derived_definition_id = sp.derived_definition_id
                       AND dps.parameter_id = r.parameter_id
                      ORDER BY r.site_id, r.time",
                    [id.into()],
                ))
                .await;

            match timestamps {
                Ok(rows) => {
                    let total = rows.len();
                    for (i, row) in rows.iter().enumerate() {
                        let site_id: Uuid = match row.try_get("", "site_id") {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let time: chrono::DateTime<chrono::FixedOffset> =
                            match row.try_get("", "time") {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                        let utc_time = time.with_timezone(&chrono::Utc);
                        if let Err(e) =
                            crate::routes::private::sensor_calibrations::services::recalculate_derived_at_timestamp(
                                &db, site_id, utc_time,
                            )
                            .await
                        {
                            tracing::error!(error = %e, time = %time, "Failed to recompute derived value");
                        }
                        if (i + 1) % 1000 == 0 {
                            tracing::info!("Recomputed {}/{} timestamps", i + 1, total);
                        }
                    }
                    tracing::info!(derived_id = %id, total = total, "Derived parameter recomputation complete");
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to query timestamps for recomputation");
                }
            }
        }).await {
            Ok(()) => {}
            Err(_) => tracing::error!(derived_id = %id, "Recompute derived task timed out after 10 minutes"),
        }
    });
    Ok(Json(serde_json::json!({ "status": "triggered" })))
}
