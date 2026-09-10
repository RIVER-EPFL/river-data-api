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
use sea_orm::Set;
use uuid::Uuid;

use super::models::{
    ActiveModel, Annotation, AnnotationOutcome, RegisterAnnotationsRequest,
    RegisterAnnotationsResponse,
};
use super::service::{slots_by_stream, stored_by_source_key};
use crate::common::AppState;
use crate::error::{AppError, AppResult};

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

    let stream_ids: Vec<Uuid> = payload.annotations.iter().map(|a| a.stream_id).collect();
    let slot_by_stream = slots_by_stream(db, &stream_ids).await?;
    let keys: Vec<String> = payload
        .annotations
        .iter()
        .map(|a| a.source_key.clone())
        .collect();
    let stored = stored_by_source_key(db, &source_system, &keys).await?;

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
        let withheld = stored
            .is_some_and(|a| a.text != item.text || a.standard_curve_id != item.standard_curve_id);
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
