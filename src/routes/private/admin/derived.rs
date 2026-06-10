use axum::{
    Json,
    extract::{Path, State},
};
use sea_orm::{ConnectionTrait, Statement};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::AppResult;

/// Recompute every derived value for a given derived parameter definition. Backfills via
/// joining source readings; tracked as a `reprocessing_jobs` row. Refreshes continuous
/// aggregates on completion. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/actions/derived_parameters/{id}/recompute",
    params(("id" = Uuid, Path, description = "Derived parameter definition UUID")),
    responses(
        (status = 200, description = "Background recompute job triggered with job_id"),
        (status = 404, description = "Derived parameter definition not found"),
    ),
    tag = "actions"
)]
pub async fn recompute_derived(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    let job_id = spawn_recompute_derived(&state.db, state.events.clone(), id).await?;
    Ok(Json(serde_json::json!({ "status": "triggered", "job_id": job_id })))
}

/// Spawn a tracked `derived_recompute` job for one derived parameter definition. Shared by the
/// recompute action handler and the job-rerun dispatcher.
pub async fn spawn_recompute_derived(
    db: &sea_orm::DatabaseConnection,
    events: crate::common::EventSender,
    id: Uuid,
) -> Result<Uuid, sea_orm::DbErr> {
    crate::routes::private::reprocessing_jobs::lifecycle::spawn_tracked_job_ctx(
        db,
        None,
        "derived_recompute",
        Some(id),
        events,
        move |ctx| async move {
            // Bound the recompute so a stuck job can't run forever; a timeout marks it failed.
            let work = async {
                tracing::info!(derived_id = %id, job_id = %ctx.job_id(), "Recomputing derived parameter");
                let rows = ctx
                    .db()
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
                    .await?;

                let total = i32::try_from(rows.len()).unwrap_or(i32::MAX);
                ctx.set_progress(0, Some(total)).await;

                let mut filled: i32 = 0;
                let mut min_filled: Option<chrono::DateTime<chrono::Utc>> = None;
                for (i, row) in rows.iter().enumerate() {
                    let Ok(site_id) = row.try_get::<Uuid>("", "site_id") else {
                        continue;
                    };
                    let Ok(time) = row.try_get::<chrono::DateTime<chrono::FixedOffset>>("", "time")
                    else {
                        continue;
                    };
                    let utc_time = time.with_timezone(&chrono::Utc);
                    match crate::routes::private::sensor_calibrations::services::recalculate_derived_at_timestamp(
                        ctx.db(), site_id, utc_time,
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
                        ctx.set_progress(i as i32 + 1, Some(total)).await;
                    }
                }

                if let Some(since) = min_filled {
                    tracing::info!(%since, "Refreshing continuous aggregates after derived recompute");
                    crate::common::sync_state::refresh_continuous_aggregates(ctx.db(), Some(since)).await;
                }
                ctx.set_progress(total, Some(total)).await;
                tracing::info!(derived_id = %id, total, filled, "Derived parameter recomputation complete");
                Ok::<i64, sea_orm::DbErr>(i64::from(filled))
            };

            match tokio::time::timeout(std::time::Duration::from_secs(600), work).await {
                Ok(res) => res,
                Err(_) => Err(sea_orm::DbErr::Custom(
                    "Timed out after 10 minutes".to_string(),
                )),
            }
        },
    )
    .await
}
