use axum::{
    Json,
    extract::{Path, Query, State},
};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::source_mappings;
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize)]
pub struct SourceMappingQuery {
    pub entity_type: Option<String>,
    pub source_system: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SourceMappingResponse {
    pub entity_type: String,
    pub source_key: i32,
    pub entity_id: Uuid,
    pub source_name: Option<String>,
    pub source_system: Option<String>,
}

impl From<source_mappings::Model> for SourceMappingResponse {
    fn from(m: source_mappings::Model) -> Self {
        Self {
            entity_type: m.entity_type,
            source_key: m.source_key,
            entity_id: m.entity_id,
            source_name: m.source_name,
            source_system: m.source_system,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct UpsertSourceMappingRequest {
    pub entity_type: String,
    pub source_key: i32,
    pub entity_id: Uuid,
    pub source_name: Option<String>,
    pub source_system: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateSourceMappingRequest {
    pub entity_id: Uuid,
    pub source_name: Option<String>,
    pub source_system: Option<String>,
}

pub async fn list_source_mappings(
    State(state): State<AppState>,
    Query(query): Query<SourceMappingQuery>,
) -> AppResult<Json<Vec<SourceMappingResponse>>> {
    let mut finder = source_mappings::Entity::find();

    if let Some(ref entity_type) = query.entity_type {
        finder = finder.filter(source_mappings::Column::EntityType.eq(entity_type.as_str()));
    }
    if let Some(ref source_system) = query.source_system {
        finder = finder.filter(source_mappings::Column::SourceSystem.eq(source_system.as_str()));
    }

    let mappings = finder.all(&state.db).await?;
    let response: Vec<SourceMappingResponse> = mappings.into_iter().map(Into::into).collect();
    Ok(Json(response))
}

pub async fn upsert_source_mapping(
    State(state): State<AppState>,
    Json(payload): Json<UpsertSourceMappingRequest>,
) -> AppResult<Json<SourceMappingResponse>> {
    let model = source_mappings::ActiveModel {
        entity_type: Set(payload.entity_type.clone()),
        source_key: Set(payload.source_key),
        entity_id: Set(payload.entity_id),
        source_name: Set(payload.source_name.clone()),
        source_system: Set(payload.source_system.clone()),
    };

    // Try insert, on conflict update
    let _result = source_mappings::Entity::insert(model)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                source_mappings::Column::EntityType,
                source_mappings::Column::SourceKey,
            ])
            .update_columns([
                source_mappings::Column::EntityId,
                source_mappings::Column::SourceName,
                source_mappings::Column::SourceSystem,
            ])
            .to_owned(),
        )
        .exec(&state.db)
        .await?;

    // Re-fetch the upserted row
    let inserted = source_mappings::Entity::find_by_id((
        payload.entity_type.clone(),
        payload.source_key,
    ))
    .one(&state.db)
    .await?
    .ok_or_else(|| AppError::Internal("Failed to fetch upserted source mapping".to_string()))?;

    Ok(Json(inserted.into()))
}

pub async fn update_source_mapping(
    State(state): State<AppState>,
    Path((entity_type, source_key)): Path<(String, i32)>,
    Json(payload): Json<UpdateSourceMappingRequest>,
) -> AppResult<Json<SourceMappingResponse>> {
    let existing = source_mappings::Entity::find_by_id((entity_type.clone(), source_key))
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Source mapping not found".to_string()))?;

    let mut active: source_mappings::ActiveModel = existing.into();
    active.entity_id = Set(payload.entity_id);
    active.source_name = Set(payload.source_name);
    active.source_system = Set(payload.source_system);

    let updated = active.update(&state.db).await?;
    Ok(Json(updated.into()))
}
