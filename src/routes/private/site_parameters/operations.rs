use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ColumnTrait, ConnectionTrait, Condition, DatabaseConnection, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect, ActiveModelTrait, Set, Statement};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use super::model::{Column, Entity, SiteParameter};

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

        let parameter = crate::routes::private::parameters::Entity::find_by_id(entity.parameter_id)
            .one(db)
            .await
            .map_err(ApiError::database)?;

        let Some(parameter) = parameter else {
            return Ok(());
        };

        // Only create if at least one default threshold is set
        if parameter.default_warning_min.is_none()
            && parameter.default_warning_max.is_none()
            && parameter.default_alarm_min.is_none()
            && parameter.default_alarm_max.is_none()
        {
            return Ok(());
        }

        // Skip if an alarm_threshold already exists for this parameter + site
        let existing = crate::routes::private::alarm_thresholds::Entity::find()
            .filter(crate::routes::private::alarm_thresholds::Column::ParameterId.eq(entity.parameter_id))
            .filter(crate::routes::private::alarm_thresholds::Column::SiteId.eq(entity.site_id))
            .one(db)
            .await
            .map_err(ApiError::database)?;

        if existing.is_some() {
            return Ok(());
        }

        let threshold = crate::routes::private::alarm_thresholds::ActiveModel {
            id: Set(Uuid::new_v4()),
            parameter_id: Set(entity.parameter_id),
            site_id: Set(Some(entity.site_id)),
            alarm_type: Set("range".to_string()),
            warning_min: Set(parameter.default_warning_min),
            warning_max: Set(parameter.default_warning_max),
            alarm_min: Set(parameter.default_alarm_min),
            alarm_max: Set(parameter.default_alarm_max),
            description: Set(Some(format!(
                "Auto-created from {} defaults",
                parameter.name
            ))),
            string_alarm_values: Set(None),
            string_warning_values: Set(None),
            created_at: Set(None),
            updated_at: Set(None),
        };

        threshold.insert(db).await.map_err(ApiError::database)?;

        Ok(())
    }
}
