use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};
use uuid::Uuid;

use super::model;
use crate::error::AppError;

/// Get or create an "api" stream for a given (site_id, parameter_id) pair.
///
/// Used by batch insert endpoints to assign a stream_id to API-submitted readings.
/// Upserts on (source_system="api", source_key="{site_id}:{parameter_id}").
/// The slot a (site, parameter) pair names, or `None` when the parameter is not assigned to the
/// site. It is the one place a reading's attribution comes from.
pub async fn site_parameter_of(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Result<Option<Uuid>, AppError> {
    use sea_orm::{ConnectionTrait, Statement};
    Ok(db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM site_parameters WHERE site_id = $1 AND parameter_id = $2 LIMIT 1",
            [site_id.into(), parameter_id.into()],
        ))
        .await?
        .and_then(|row| row.try_get::<Uuid>("", "id").ok()))
}

pub async fn get_or_create_api_stream(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Result<Uuid, AppError> {
    let source_key = format!("{site_id}:{parameter_id}");

    // Try to find existing
    if let Some(stream) = model::Entity::find()
        .filter(model::Column::SourceSystem.eq("api"))
        .filter(model::Column::SourceKey.eq(&source_key))
        .one(db)
        .await?
    {
        return Ok(stream.id);
    }

    // Create new
    let now = chrono::Utc::now();
    let id = Uuid::new_v4();
    let site_parameter_id = site_parameter_of(db, site_id, parameter_id).await?;
    let active_model = model::ActiveModel {
        id: Set(id),
        source_system: Set("api".to_string()),
        source_key: Set(source_key.clone()),
        source_name: Set(Some("API batch insert".to_string())),
        source_path: Set(None),
        metadata: Set(serde_json::json!({})),
        // Paired on creation: this channel exists to carry one slot's readings, and attribution is
        // read from the pairing rather than restated per row. A slot that has no `site_parameters`
        // row yet leaves the stream unpaired, like any other undiscovered channel.
        site_parameter_id: Set(site_parameter_id),
        paired_at: Set(site_parameter_id.map(|_| now.into())),
        sensor_id: Set(None),
        measurement_type: Set(None),
        is_active: Set(true),
        discovered_at: Set(now.into()),
        last_data_time: Set(None),
        pairing_plan_id: Set(None),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    };

    model::Entity::insert(active_model)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                model::Column::SourceSystem,
                model::Column::SourceKey,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(db)
        .await
        .map_err(AppError::Database)?;

    // Re-fetch in case of race condition (ON CONFLICT DO NOTHING returns no id)
    let stream = model::Entity::find()
        .filter(model::Column::SourceSystem.eq("api"))
        .filter(model::Column::SourceKey.eq(&source_key))
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to create API stream".to_string()))?;

    Ok(stream.id)
}
