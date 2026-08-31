//! Provenance-keyed upsert of source-authored annotations for sync services. Idempotent per
//! `(source_system, source_key)`: a full-content pass re-asserting a key updates the row in
//! place, so the source may send its complete set every cycle. The site and parameter come from
//! the stream's pairing, never from the request; an annotation on an unpaired stream is refused
//! per item as `unpaired` and lands once the source re-asserts it after pairing.

use axum::{Json, extract::State};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Statement};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::{data_streams, sites::parameters as site_parameters};

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterAnnotationsRequest {
    /// The sync source the annotations come from, e.g. "cnet".
    pub source_system: String,
    pub annotations: Vec<AnnotationItem>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AnnotationItem {
    /// The annotation's identity within the source; the upsert key is
    /// (source_system, source_key).
    pub source_key: String,
    /// The stream whose pairing resolves the annotation's site and parameter.
    pub stream_id: Uuid,
    /// The instant the annotation covers, stored as a point (start_time == end_time).
    pub time: chrono::DateTime<chrono::Utc>,
    pub category: String,
    pub text: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterAnnotationsResponse {
    pub annotations: Vec<AnnotationOutcome>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AnnotationOutcome {
    pub source_key: String,
    /// None when the annotation was not stored (`unpaired`).
    pub id: Option<Uuid>,
    /// created | updated | unchanged | unpaired
    pub status: String,
}

#[utoipa::path(
    post,
    path = "/annotations/register",
    request_body = RegisterAnnotationsRequest,
    responses((status = 200, body = RegisterAnnotationsResponse)),
    tag = "annotations"
)]
pub async fn register_annotations(
    State(state): State<AppState>,
    Json(payload): Json<RegisterAnnotationsRequest>,
) -> AppResult<Json<RegisterAnnotationsResponse>> {
    if payload.source_system.trim().is_empty() {
        return Err(AppError::BadRequest("source_system must not be empty".into()));
    }
    let db = &state.db;

    // One slot lookup per distinct stream: stream → site_parameter → (site_id, parameter_id).
    let mut stream_ids: Vec<Uuid> = payload.annotations.iter().map(|a| a.stream_id).collect();
    stream_ids.sort_unstable();
    stream_ids.dedup();
    let streams = data_streams::Entity::find()
        .filter(data_streams::Column::Id.is_in(stream_ids.clone()))
        .all(db)
        .await?;
    let mut sp_ids: Vec<Uuid> = streams.iter().filter_map(|s| s.site_parameter_id).collect();
    sp_ids.sort_unstable();
    sp_ids.dedup();
    let slots = site_parameters::Entity::find()
        .filter(site_parameters::Column::Id.is_in(sp_ids))
        .all(db)
        .await?;
    let slot_by_id: HashMap<Uuid, (Uuid, Uuid)> = slots
        .iter()
        .map(|sp| (sp.id, (sp.site_id, sp.parameter_id)))
        .collect();
    let slot_by_stream: HashMap<Uuid, (Uuid, Uuid)> = streams
        .iter()
        .filter_map(|s| {
            s.site_parameter_id
                .and_then(|sp| slot_by_id.get(&sp))
                .map(|slot| (s.id, *slot))
        })
        .collect();

    let mut outcomes = Vec::with_capacity(payload.annotations.len());
    for item in &payload.annotations {
        if item.source_key.trim().is_empty() {
            return Err(AppError::BadRequest("source_key must not be empty".into()));
        }
        let Some((site_id, parameter_id)) = slot_by_stream.get(&item.stream_id).copied() else {
            outcomes.push(AnnotationOutcome {
                source_key: item.source_key.clone(),
                id: None,
                status: "unpaired".into(),
            });
            continue;
        };
        // Single-statement upsert: the DO UPDATE's WHERE makes an identical re-assert return no
        // row (unchanged), and `xmax = 0` distinguishes an insert from an update.
        let row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "INSERT INTO annotations
                     (id, site_id, parameter_id, start_time, end_time, text, category,
                      created_by, source_system, source_key, created_at)
                 VALUES ($1, $2, $3, $4, $4, $5, $6, $7, $8, $9, NOW())
                 ON CONFLICT (source_system, source_key)
                     WHERE source_system IS NOT NULL AND source_key IS NOT NULL
                     DO UPDATE SET site_id = EXCLUDED.site_id,
                                   parameter_id = EXCLUDED.parameter_id,
                                   start_time = EXCLUDED.start_time,
                                   end_time = EXCLUDED.end_time,
                                   text = EXCLUDED.text,
                                   category = EXCLUDED.category
                     WHERE (annotations.site_id, annotations.parameter_id,
                            annotations.start_time, annotations.end_time,
                            annotations.text, annotations.category)
                           IS DISTINCT FROM
                           (EXCLUDED.site_id, EXCLUDED.parameter_id,
                            EXCLUDED.start_time, EXCLUDED.end_time,
                            EXCLUDED.text, EXCLUDED.category)
                 RETURNING id, (xmax = 0) AS created",
                [
                    Uuid::new_v4().into(),
                    site_id.into(),
                    parameter_id.into(),
                    sea_orm::prelude::DateTimeWithTimeZone::from(item.time).into(),
                    item.text.clone().into(),
                    item.category.clone().into(),
                    format!("sync:{}", payload.source_system).into(),
                    payload.source_system.clone().into(),
                    item.source_key.clone().into(),
                ],
            ))
            .await?;
        let outcome = match row {
            Some(row) => AnnotationOutcome {
                source_key: item.source_key.clone(),
                id: Some(row.try_get::<Uuid>("", "id")?),
                status: if row.try_get::<bool>("", "created")? {
                    "created".into()
                } else {
                    "updated".into()
                },
            },
            None => {
                let existing = db
                    .query_one(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "SELECT id FROM annotations
                         WHERE source_system = $1 AND source_key = $2",
                        [
                            payload.source_system.clone().into(),
                            item.source_key.clone().into(),
                        ],
                    ))
                    .await?
                    .ok_or_else(|| {
                        AppError::Internal(format!(
                            "annotation upsert for {} returned no row and no stored row exists",
                            item.source_key
                        ))
                    })?;
                AnnotationOutcome {
                    source_key: item.source_key.clone(),
                    id: Some(existing.try_get::<Uuid>("", "id")?),
                    status: "unchanged".into(),
                }
            }
        };
        outcomes.push(outcome);
    }

    Ok(Json(RegisterAnnotationsResponse {
        annotations: outcomes,
    }))
}
