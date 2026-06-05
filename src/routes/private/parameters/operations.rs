use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set,
};
use uuid::Uuid;

use super::model::Parameter;

pub struct ParameterOperations;

#[async_trait]
impl CRUDOperations for ParameterOperations {
    type Resource = Parameter;

    async fn after_update(
        &self,
        db: &DatabaseConnection,
        entity: &mut Parameter,
    ) -> Result<(), ApiError> {
        let has_defaults = entity.default_warning_min.is_some()
            || entity.default_warning_max.is_some()
            || entity.default_alarm_min.is_some()
            || entity.default_alarm_max.is_some();

        if !has_defaults {
            return Ok(());
        }

        let site_params = crate::routes::private::site_parameters::Entity::find()
            .filter(crate::routes::private::site_parameters::Column::ParameterId.eq(entity.id))
            .filter(crate::routes::private::site_parameters::Column::IsActive.eq(true))
            .all(db)
            .await
            .map_err(ApiError::database)?;

        for sp in site_params {
            let existing = crate::routes::private::alarm_thresholds::Entity::find()
                .filter(
                    crate::routes::private::alarm_thresholds::Column::ParameterId
                        .eq(entity.id),
                )
                .filter(
                    crate::routes::private::alarm_thresholds::Column::SiteId.eq(sp.site_id),
                )
                .one(db)
                .await
                .map_err(ApiError::database)?;

            if existing.is_none() {
                let threshold = crate::routes::private::alarm_thresholds::ActiveModel {
                    id: Set(Uuid::new_v4()),
                    parameter_id: Set(entity.id),
                    site_id: Set(Some(sp.site_id)),
                    warning_min: Set(entity.default_warning_min),
                    warning_max: Set(entity.default_warning_max),
                    alarm_min: Set(entity.default_alarm_min),
                    alarm_max: Set(entity.default_alarm_max),
                    description: Set(Some(format!(
                        "Auto-created from {} defaults",
                        entity.name
                    ))),
                    created_at: Set(None),
                    updated_at: Set(None),
                };

                threshold.insert(db).await.map_err(ApiError::database)?;
            }
        }

        Ok(())
    }
}
