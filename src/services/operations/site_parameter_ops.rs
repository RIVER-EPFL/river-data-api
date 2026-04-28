use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, ActiveModelTrait, Set};
use uuid::Uuid;

use crate::entity::site_parameters::SiteParameter;

pub struct SiteParameterOperations;

#[async_trait]
impl CRUDOperations for SiteParameterOperations {
    type Resource = SiteParameter;

    async fn after_create(
        &self,
        db: &DatabaseConnection,
        entity: &mut SiteParameter,
    ) -> Result<(), ApiError> {
        // Look up the parent parameter's default thresholds
        let parameter = crate::entity::parameters::Entity::find_by_id(entity.parameter_id)
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
        let existing = crate::entity::alarm_thresholds::Entity::find()
            .filter(crate::entity::alarm_thresholds::Column::ParameterId.eq(entity.parameter_id))
            .filter(crate::entity::alarm_thresholds::Column::SiteId.eq(entity.site_id))
            .one(db)
            .await
            .map_err(ApiError::database)?;

        if existing.is_some() {
            return Ok(());
        }

        let threshold = crate::entity::alarm_thresholds::ActiveModel {
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
