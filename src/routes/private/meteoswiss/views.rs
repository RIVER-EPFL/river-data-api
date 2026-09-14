//! The station picker's reader: what a site form calls to name a station.

use axum::{
    Json,
    extract::{Query, State},
};
use serde::Deserialize;
use uuid::Uuid;

use super::models::StationCandidate;
use super::service::{rank_stations, search_stations, site_origin};
use crate::common::AppState;
use crate::error::AppResult;

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct StationQuery {
    /// Part of an abbreviation or a name. Absent lists every station.
    pub q: Option<String>,
    /// The site being configured, whose coordinates rank the candidates.
    pub site_id: Option<Uuid>,
}

/// The stations a site may subscribe to, nearest first. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/meteoswiss/stations",
    params(StationQuery),
    responses((status = 200, description = "Candidate stations, nearest first", body = [StationCandidate])),
    tag = "meteoswiss"
)]
pub async fn list_stations(
    State(state): State<AppState>,
    Query(q): Query<StationQuery>,
) -> AppResult<Json<Vec<StationCandidate>>> {
    let matching = search_stations(&state.db, q.q.as_deref()).await?;
    let origin = match q.site_id {
        Some(site_id) => site_origin(&state.db, site_id).await?,
        None => None,
    };
    Ok(Json(rank_stations(matching, origin)))
}
