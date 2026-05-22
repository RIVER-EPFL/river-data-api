use axum::{
    Json,
    extract::{Path, State},
};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::AppResult;

async fn update_job(
    db: &DatabaseConnection,
    job_id: Uuid,
    sql: &str,
    values: Vec<sea_orm::Value>,
) {
    if let Err(e) = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await
    {
        tracing::warn!(error = %e, %job_id, "Failed to update reprocessing job");
    }
}

pub async fn recompute_derived(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    let db = state.db.clone();
    let job_id = Uuid::new_v4();
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO reprocessing_jobs (id, sensor_id, trigger_type, trigger_id, status) \
         VALUES ($1, NULL, 'derived_recompute', $2, 'pending')",
        [job_id.into(), id.into()],
    ))
    .await?;

    tokio::spawn(async move {
        match tokio::time::timeout(std::time::Duration::from_secs(600), async {
            tracing::info!(derived_id = %id, %job_id, "Recomputing derived parameter");
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
                    let total = rows.len() as i32;
                    update_job(
                        &db,
                        job_id,
                        "UPDATE reprocessing_jobs SET status = 'running', total = $1, progress = 0 WHERE id = $2",
                        vec![total.into(), job_id.into()],
                    )
                    .await;

                    let mut filled: i32 = 0;
                    let mut min_filled: Option<chrono::DateTime<chrono::Utc>> = None;
                    for (i, row) in rows.iter().enumerate() {
                        let Ok(site_id) = row.try_get::<Uuid>("", "site_id") else { continue };
                        let Ok(time) = row.try_get::<chrono::DateTime<chrono::FixedOffset>>("", "time") else { continue };
                        let utc_time = time.with_timezone(&chrono::Utc);
                        match crate::routes::private::sensor_calibrations::services::recalculate_derived_at_timestamp(
                            &db, site_id, utc_time,
                        )
                        .await
                        {
                            Ok(()) => {
                                filled += 1;
                                min_filled = Some(min_filled.map_or(utc_time, |m| m.min(utc_time)));
                            }
                            Err(e) => tracing::error!(error = %e, time = %time, "Failed to recompute derived value"),
                        }
                        if (i + 1) % 500 == 0 {
                            update_job(
                                &db,
                                job_id,
                                "UPDATE reprocessing_jobs SET progress = $1 WHERE id = $2",
                                vec![(i as i32 + 1).into(), job_id.into()],
                            )
                            .await;
                        }
                    }

                    if let Some(since) = min_filled {
                        tracing::info!(%since, %job_id, "Refreshing continuous aggregates after derived recompute");
                        crate::common::sync_state::refresh_continuous_aggregates(&db, Some(since)).await;
                    }

                    update_job(
                        &db,
                        job_id,
                        "UPDATE reprocessing_jobs \
                         SET status = 'completed', progress = total, readings_updated = $1, completed_at = NOW() \
                         WHERE id = $2",
                        vec![filled.into(), job_id.into()],
                    )
                    .await;
                    tracing::info!(derived_id = %id, %job_id, total, filled, "Derived parameter recomputation complete");
                }
                Err(e) => {
                    let msg = e.to_string();
                    tracing::error!(error = %e, %job_id, "Failed to query timestamps for recomputation");
                    update_job(
                        &db,
                        job_id,
                        "UPDATE reprocessing_jobs SET status = 'failed', error_message = $1, completed_at = NOW() WHERE id = $2",
                        vec![msg.as_str().into(), job_id.into()],
                    )
                    .await;
                }
            }
        }).await {
            Ok(()) => {}
            Err(_) => {
                tracing::error!(derived_id = %id, %job_id, "Recompute derived task timed out after 10 minutes");
                update_job(
                    &db,
                    job_id,
                    "UPDATE reprocessing_jobs SET status = 'failed', error_message = 'Timed out after 10 minutes', completed_at = NOW() WHERE id = $1",
                    vec![job_id.into()],
                )
                .await;
            }
        }
    });
    Ok(Json(serde_json::json!({ "status": "triggered", "job_id": job_id })))
}
