//! Provenance-keyed upsert of source-authored site notes for sync services. Idempotent per
//! `(source_system, source_key)`, so a source may send its complete set every cycle.
//!
//! The site comes from the source's own station name, resolved against sites that already exist.
//! A note mints nothing: a station river-data has never seen is reported `unresolved` and lands on
//! a later cycle, once pairing has created the site.

use axum::{Json, extract::State};
use crudcrate::UpsertStatus;
use sea_orm::{ConnectionTrait, Set, Statement};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{ActiveModel, Note};
use crate::common::AppState;
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterNotesRequest {
    /// The sync source the notes come from, e.g. "metalp".
    pub source_system: String,
    pub notes: Vec<NoteItem>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NoteItem {
    /// The note's identity within the source, e.g. "notes:1"; the upsert key is
    /// (source_system, source_key).
    pub source_key: String,
    /// The source's own station name. Resolved case-insensitively against existing sites.
    pub site_name: String,
    pub text: String,
    #[serde(default)]
    pub verified: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterNotesResponse {
    pub notes: Vec<NoteOutcome>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct NoteOutcome {
    pub source_key: String,
    /// None when the note was not stored (`unresolved`).
    #[schema(required)]
    pub id: Option<Uuid>,
    /// created | updated | unchanged | unresolved
    pub status: String,
}

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
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(payload): Json<RegisterNotesRequest>,
) -> AppResult<Json<RegisterNotesResponse>> {
    let source_system = crate::common::provenance::source_system(&auth, &payload.source_system)?;
    let db = &state.db;

    let mut names: Vec<String> = payload
        .notes
        .iter()
        .map(|n| n.site_name.trim().to_lowercase())
        .collect();
    names.sort();
    names.dedup();
    let mut site_by_name: HashMap<String, Uuid> = HashMap::new();
    if !names.is_empty() {
        let rows = db
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT id, LOWER(name) AS lower_name FROM sites WHERE LOWER(name) = ANY($1)",
                [names.into()],
            ))
            .await?;
        for row in rows {
            site_by_name.insert(row.try_get("", "lower_name")?, row.try_get("", "id")?);
        }
    }

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
