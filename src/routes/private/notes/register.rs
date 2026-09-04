//! Provenance-keyed upsert of source-authored site notes for sync services. Idempotent per
//! `(source_system, source_key)`, so a source may send its complete set every cycle.
//!
//! The site comes from the source's own station name, resolved against sites that already exist.
//! A note mints nothing: a station river-data has never seen is reported `unresolved` and lands on
//! a later cycle, once pairing has created the site.

use axum::{Json, extract::State};
use sea_orm::{ConnectionTrait, Statement};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

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
    Json(payload): Json<RegisterNotesRequest>,
) -> AppResult<Json<RegisterNotesResponse>> {
    if payload.source_system.trim().is_empty() {
        return Err(AppError::BadRequest(
            "source_system must not be empty".into(),
        ));
    }
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

        // Single-statement upsert: the DO UPDATE's WHERE makes an identical re-assert return no
        // row (unchanged), and `xmax = 0` distinguishes an insert from an update.
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "INSERT INTO notes
                     (id, site_id, text, verified, created_by, source_system, source_key,
                      created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, NOW(), NOW())
                 ON CONFLICT (source_system, source_key)
                     WHERE source_system IS NOT NULL AND source_key IS NOT NULL
                     DO UPDATE SET site_id = EXCLUDED.site_id,
                                   text = EXCLUDED.text,
                                   verified = EXCLUDED.verified,
                                   updated_at = NOW()
                     WHERE (notes.site_id, notes.text, notes.verified)
                           IS DISTINCT FROM
                           (EXCLUDED.site_id, EXCLUDED.text, EXCLUDED.verified)
                 RETURNING id, (xmax = 0) AS created",
                [
                    Uuid::new_v4().into(),
                    site_id.into(),
                    item.text.clone().into(),
                    item.verified.into(),
                    format!("sync:{}", payload.source_system).into(),
                    payload.source_system.clone().into(),
                    item.source_key.clone().into(),
                ],
            ))
            .await?;

        let outcome = match row {
            Some(row) => NoteOutcome {
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
                    .query_one_raw(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "SELECT id FROM notes WHERE source_system = $1 AND source_key = $2",
                        [
                            payload.source_system.clone().into(),
                            item.source_key.clone().into(),
                        ],
                    ))
                    .await?
                    .ok_or_else(|| {
                        AppError::Internal(format!(
                            "note upsert for {} returned no row and no stored row exists",
                            item.source_key
                        ))
                    })?;
                NoteOutcome {
                    source_key: item.source_key.clone(),
                    id: Some(existing.try_get::<Uuid>("", "id")?),
                    status: "unchanged".into(),
                }
            }
        };
        outcomes.push(outcome);
    }

    Ok(Json(RegisterNotesResponse { notes: outcomes }))
}
