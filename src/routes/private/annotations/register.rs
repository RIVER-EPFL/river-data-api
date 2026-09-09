//! Provenance-keyed upsert of source-authored annotations for sync services. Idempotent per
//! `(source_system, source_key)`: a full-content pass re-asserting a key updates the row in
//! place, so the source may send its complete set every cycle. The site and parameter come from
//! the stream's pairing, never from the request; an annotation on an unpaired stream is refused
//! per item as `unpaired` and lands once the source re-asserts it after pairing.
//!
//! An annotation naming a `standard_curve_id` records which curve produced a source-corrected
//! value. Once stored with one, its curve and text are frozen (`frozen`): the source's later text
//! describes the curve as it is now, not the curve the value was made with.

use axum::{Json, extract::State};
use crudcrate::UpsertStatus;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{ActiveModel, Annotation, Column, Entity, Model};
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
    /// The standard curve the source applied to produce the annotated value, when it did.
    #[serde(default)]
    pub standard_curve_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterAnnotationsResponse {
    pub annotations: Vec<AnnotationOutcome>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AnnotationOutcome {
    pub source_key: String,
    /// None when the annotation was not stored (`unpaired`).
    #[schema(required)]
    pub id: Option<Uuid>,
    /// created | updated | unchanged | frozen | unpaired
    pub status: String,
}

#[utoipa::path(
    post,
    path = "/api/annotations/register",
    request_body = RegisterAnnotationsRequest,
    responses((status = 200, body = RegisterAnnotationsResponse)),
    tag = "annotations"
)]
pub async fn register_annotations(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(payload): Json<RegisterAnnotationsRequest>,
) -> AppResult<Json<RegisterAnnotationsResponse>> {
    let source_system = crate::common::provenance::source_system(&auth, &payload.source_system)?;
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

    // The stored rows this pass re-asserts, read once: a row that already names a curve is
    // frozen, and which half the source moved is what the outcome reports.
    let mut keys: Vec<String> = payload
        .annotations
        .iter()
        .map(|a| a.source_key.clone())
        .collect();
    keys.sort_unstable();
    keys.dedup();
    let stored: HashMap<String, Model> = Entity::find()
        .filter(Column::SourceSystem.eq(source_system.clone()))
        .filter(Column::SourceKey.is_in(keys))
        .all(db)
        .await?
        .into_iter()
        .filter_map(|a| a.source_key.clone().map(|key| (key, a)))
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
        let stored = stored.get(&item.source_key);
        // A row that already names a curve keeps the curve and the text the value was made with,
        // so the registration moves only its slot, instant and category; `frozen` reports the text
        // or curve the source sent and the row did not take.
        let frozen = stored.is_some_and(|a| a.standard_curve_id.is_some());
        let mut active = ActiveModel {
            site_id: Set(site_id),
            parameter_id: Set(parameter_id),
            start_time: Set(item.time),
            end_time: Set(item.time),
            category: Set(item.category.clone()),
            created_by: Set(Some(format!("sync:{source_system}"))),
            source_system: Set(Some(source_system.clone())),
            source_key: Set(Some(item.source_key.clone())),
            standard_curve_id: Set(item.standard_curve_id),
            ..Default::default()
        };
        if !frozen {
            active.text = Set(item.text.clone());
        }
        let (annotation, status) =
            crate::common::provenance::register::<Annotation>(db, active).await?;
        // The curve is excluded from the create model, so an upsert never writes one onto a
        // stored row; a row that names none takes the curve of the first pass that does.
        let adopts_curve = stored.is_some_and(|a| a.standard_curve_id.is_none())
            && item.standard_curve_id.is_some();
        if adopts_curve {
            let mut claim = ActiveModel {
                id: sea_orm::ActiveValue::Unchanged(annotation.id),
                ..Default::default()
            };
            claim.standard_curve_id = Set(item.standard_curve_id);
            sea_orm::ActiveModelTrait::update(claim, db).await?;
        }
        let withheld = stored.is_some_and(|a| {
            a.text != item.text || a.standard_curve_id != item.standard_curve_id
        });
        let outcome = AnnotationOutcome {
            source_key: item.source_key.clone(),
            id: Some(annotation.id),
            status: if frozen && withheld {
                "frozen".into()
            } else if adopts_curve {
                "updated".into()
            } else {
                match status {
                    UpsertStatus::Created => "created".into(),
                    UpsertStatus::Updated => "updated".into(),
                    UpsertStatus::Unchanged => "unchanged".into(),
                }
            },
        };
        outcomes.push(outcome);
    }

    Ok(Json(RegisterAnnotationsResponse {
        annotations: outcomes,
    }))
}
