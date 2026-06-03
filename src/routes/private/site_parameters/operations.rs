use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ColumnTrait, ConnectionTrait, Condition, DatabaseConnection, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect, ActiveModelTrait, Set, Statement};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use super::model::{Column, Entity, SiteParameter};
use crate::routes::private::sensor_calibrations::services::{
    recalculate_derived_at_timestamp, spawn_tracked_job,
};

async fn enrich(db: &DatabaseConnection, items: &mut [SiteParameter]) -> Result<(), ApiError> {
    if items.is_empty() {
        return Ok(());
    }

    let param_ids: HashSet<Uuid> = items.iter().map(|sp| sp.parameter_id).collect();
    let def_ids: HashSet<Uuid> = items
        .iter()
        .filter_map(|sp| sp.derived_definition_id)
        .collect();

    if !param_ids.is_empty() {
        let params = crate::routes::private::parameters::Entity::find()
            .filter(crate::routes::private::parameters::Column::Id.is_in(param_ids))
            .all(db)
            .await
            .map_err(ApiError::database)?;
        let by_id: HashMap<Uuid, crate::routes::private::parameters::Parameter> = params
            .into_iter()
            .map(|m| (m.id, crate::routes::private::parameters::Parameter::from(m)))
            .collect();
        for sp in items.iter_mut() {
            if let Some(p) = by_id.get(&sp.parameter_id) {
                sp.parameter = vec![p.clone()];
            }
        }
    }

    if !def_ids.is_empty() {
        let defs = crate::routes::private::derived_parameters::definition_model::Entity::find()
            .filter(crate::routes::private::derived_parameters::definition_model::Column::Id.is_in(def_ids))
            .all(db)
            .await
            .map_err(ApiError::database)?;
        let by_id: HashMap<Uuid, crate::routes::private::derived_parameters::definition_model::DerivedParameterDefinition> = defs
            .into_iter()
            .map(|m| (m.id, crate::routes::private::derived_parameters::definition_model::DerivedParameterDefinition::from(m)))
            .collect();
        for sp in items.iter_mut() {
            if let Some(def_id) = sp.derived_definition_id
                && let Some(def) = by_id.get(&def_id)
            {
                sp.derived_definition = Some(def.clone());
            }
        }
    }

    Ok(())
}

pub struct SiteParameterOperations;

#[async_trait]
impl CRUDOperations for SiteParameterOperations {
    type Resource = SiteParameter;

    async fn fetch_one(
        &self,
        db: &DatabaseConnection,
        id: Uuid,
    ) -> Result<SiteParameter, ApiError> {
        let model = Entity::find_by_id(id)
            .one(db)
            .await
            .map_err(ApiError::database)?
            .ok_or_else(|| ApiError::not_found(SiteParameter::RESOURCE_NAME_SINGULAR, Some(id.to_string())))?;
        let mut resource = SiteParameter::from(model);
        let mut slice = std::slice::from_mut(&mut resource);
        enrich(db, &mut slice).await?;
        Ok(resource)
    }

    async fn fetch_all(
        &self,
        db: &DatabaseConnection,
        condition: &Condition,
        order_column: Column,
        order_direction: Order,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<<SiteParameter as CRUDResource>::ListModel>, ApiError> {
        let models = Entity::find()
            .filter(condition.clone())
            .order_by(order_column, order_direction)
            .offset(offset)
            .limit(limit)
            .all(db)
            .await
            .map_err(ApiError::database)?;
        let mut resources: Vec<SiteParameter> = models.into_iter().map(SiteParameter::from).collect();
        enrich(db, &mut resources).await?;
        Ok(resources
            .into_iter()
            .map(<SiteParameter as CRUDResource>::ListModel::from)
            .collect())
    }

    async fn before_delete(
        &self,
        db: &DatabaseConnection,
        id: Uuid,
    ) -> Result<(), ApiError> {
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE data_streams SET site_parameter_id = NULL WHERE site_parameter_id = $1",
            [id.into()],
        ))
        .await
        .map_err(ApiError::database)?;
        Ok(())
    }

    async fn after_create(
        &self,
        db: &DatabaseConnection,
        entity: &mut SiteParameter,
    ) -> Result<(), ApiError> {
        if entity.is_active.is_none() {
            db.execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE site_parameters SET is_active = true WHERE id = $1",
                [entity.id.into()],
            ))
            .await
            .map_err(ApiError::database)?;
            entity.is_active = Some(true);
        }
        if entity.is_public.is_none() {
            db.execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE site_parameters SET is_public = false WHERE id = $1",
                [entity.id.into()],
            ))
            .await
            .map_err(ApiError::database)?;
            entity.is_public = Some(false);
        }

        let parameter = crate::routes::private::parameters::Entity::find_by_id(entity.parameter_id)
            .one(db)
            .await
            .map_err(ApiError::database)?;

        let Some(parameter) = parameter else {
            return Ok(());
        };

        // Backfill a human-readable name from the parameter when the client omitted it.
        // `name` is fulltext/sortable, so it must not be left empty.
        if entity.name.trim().is_empty() {
            db.execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE site_parameters SET name = $1 WHERE id = $2",
                [parameter.name.clone().into(), entity.id.into()],
            ))
            .await
            .map_err(ApiError::database)?;
            entity.name = parameter.name.clone();
        }

        // Auto-create an alarm threshold from the parameter's defaults, when any are set and
        // none exists yet for this parameter + site. Derived output parameters carry no default
        // thresholds, so this is simply skipped for them — the derived backfill below still runs.
        let has_default_threshold = parameter.default_warning_min.is_some()
            || parameter.default_warning_max.is_some()
            || parameter.default_alarm_min.is_some()
            || parameter.default_alarm_max.is_some();

        if has_default_threshold {
            let existing = crate::routes::private::alarm_thresholds::Entity::find()
                .filter(crate::routes::private::alarm_thresholds::Column::ParameterId.eq(entity.parameter_id))
                .filter(crate::routes::private::alarm_thresholds::Column::SiteId.eq(entity.site_id))
                .one(db)
                .await
                .map_err(ApiError::database)?;

            if existing.is_none() {
                let threshold = crate::routes::private::alarm_thresholds::ActiveModel {
                    id: Set(Uuid::new_v4()),
                    parameter_id: Set(entity.parameter_id),
                    site_id: Set(Some(entity.site_id)),
                    warning_min: Set(parameter.default_warning_min),
                    warning_max: Set(parameter.default_warning_max),
                    alarm_min: Set(parameter.default_alarm_min),
                    alarm_max: Set(parameter.default_alarm_max),
                    description: Set(Some(format!(
                        "Auto-created from {} defaults",
                        parameter.name
                    ))),
                    created_at: Set(None),
                    updated_at: Set(None),
                };

                threshold.insert(db).await.map_err(ApiError::database)?;
            }
        }

        // Backfill derived values for the readings already present at this site when a
        // derived site_parameter is assigned. Tracked via spawn_tracked_job. A guard skips
        // the spawn if an overlapping backfill is already in flight (this definition's own
        // assignment/recompute, or a CSV import / pairing backfill that will produce the
        // same derived rows) so we don't double-run the same work.
        if entity.is_derived == Some(true)
            && let Some(def_id) = entity.derived_definition_id
            && let Some(events) = crate::common::global_event_sender()
        {
            let site_id = entity.site_id;

            let in_flight = db
                .query_one(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    r"SELECT 1
                      FROM reprocessing_jobs
                      WHERE status IN ('pending', 'running')
                        AND (
                          (trigger_type IN ('derived_assignment', 'derived_recompute') AND trigger_id = $1)
                          OR trigger_type IN ('csv_import', 'pairing_backfill')
                        )
                      LIMIT 1",
                    [def_id.into()],
                ))
                .await
                .map_err(ApiError::database)?;

            if in_flight.is_some() {
                tracing::info!(
                    %def_id, %site_id,
                    "Skipping derived assignment backfill: an overlapping reprocessing job is in flight"
                );
            } else {
                spawn_tracked_job(
                    db,
                    None,
                    "derived_assignment",
                    Some(def_id),
                    events,
                    move |db| async move {
                        tracing::info!(%def_id, %site_id, "Computing derived values after site assignment");

                        let rows = db
                            .query_all(Statement::from_sql_and_values(
                                sea_orm::DatabaseBackend::Postgres,
                                r"SELECT DISTINCT r.time
                                  FROM readings r
                                  JOIN derived_parameter_sources dps ON dps.parameter_id = r.parameter_id
                                  WHERE dps.derived_definition_id = $1 AND r.site_id = $2
                                  ORDER BY r.time",
                                [def_id.into(), site_id.into()],
                            ))
                            .await?;

                        let mut filled = 0i64;
                        let mut earliest: Option<chrono::DateTime<chrono::Utc>> = None;
                        for row in &rows {
                            let Ok(time) =
                                row.try_get::<chrono::DateTime<chrono::FixedOffset>>("", "time")
                            else {
                                continue;
                            };
                            let utc = time.with_timezone(&chrono::Utc);
                            if recalculate_derived_at_timestamp(&db, site_id, utc).await.is_ok() {
                                filled += 1;
                                earliest = Some(earliest.map_or(utc, |e| e.min(utc)));
                            }
                        }

                        if let Some(since) = earliest {
                            crate::common::sync_state::refresh_continuous_aggregates(&db, Some(since)).await;
                        }

                        tracing::info!(%def_id, %site_id, filled, "Derived assignment backfill completed");
                        Ok(filled)
                    },
                )
                .await
                .map_err(ApiError::database)?;
            }
        }

        Ok(())
    }
}
