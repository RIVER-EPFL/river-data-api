//! Provenance-keyed upsert of source-authored site notes for sync services. Idempotent per
//! `(source_system, source_key)`, so a source may send its complete set every cycle.
//!
//! The site comes from the source's own station name, resolved through the source's links and then
//! against the names of sites that already exist.
//! A note mints nothing: a station river-data has never seen is reported `unresolved` and lands on
//! a later cycle, once pairing has created the site.

use axum::{Json, extract::State};
use crudcrate::UpsertStatus;
use sea_orm::Set;

use super::models::{ActiveModel, Note, NoteOutcome, RegisterNotesRequest, RegisterNotesResponse};
use super::service::sites_by_station;
use crate::common::AppState;
use crate::error::{AppError, AppResult};

/// Register a source's site notes. Requires `write_metadata` (sync session tokens carry it).
#[utoipa::path(
    post,
    path = "/api/notes/register",
    request_body = RegisterNotesRequest,
    responses((status = 200, body = RegisterNotesResponse)),
    tag = "notes"
)]
pub async fn register_notes(
    State(state): State<AppState>,
    Json(payload): Json<RegisterNotesRequest>,
) -> AppResult<Json<RegisterNotesResponse>> {
    let source_system = crate::common::provenance::source_system(&payload.source_system)?;
    let db = &state.db;

    let names: Vec<String> = payload.notes.iter().map(|n| n.site_name.clone()).collect();
    let site_by_name = sites_by_station(db, &source_system, &names).await?;

    let mut outcomes = Vec::with_capacity(payload.notes.len());
    for item in &payload.notes {
        if item.source_key.trim().is_empty() {
            return Err(AppError::BadRequest("source_key must not be empty".into()));
        }
        let Some(site_id) = site_by_name
            .get(&item.site_name.trim().to_lowercase())
            .copied()
        else {
            outcomes.push(NoteOutcome {
                source_key: item.source_key.clone(),
                id: None,
                status: "unresolved".into(),
            });
            continue;
        };

        let active = ActiveModel {
            site_id: Set(site_id),
            text: Set(item.text.clone()),
            verified: Set(item.verified),
            created_by: Set(Some(format!("sync:{source_system}"))),
            source_system: Set(Some(source_system.clone())),
            source_key: Set(Some(item.source_key.clone())),
            ..Default::default()
        };
        let (note, status) = crate::common::provenance::register::<Note>(db, active).await?;
        let outcome = NoteOutcome {
            source_key: item.source_key.clone(),
            id: Some(note.id),
            status: match status {
                UpsertStatus::Created => "created".into(),
                UpsertStatus::Updated => "updated".into(),
                UpsertStatus::Unchanged => "unchanged".into(),
            },
        };
        outcomes.push(outcome);
    }

    Ok(Json(RegisterNotesResponse { notes: outcomes }))
}
