//! The continuous-aggregate refresh as the sync, calibration and job paths call it.
//!
//! The window arithmetic, the view list and the error handling all live in
//! [`crate::common::aggregates`]; this module is only the `Option<since>` call shape those sites
//! use. Both functions are fallible: a refresh that could not run has to reach the caller that
//! asked for it, or a tracked job reports `completed` while the rollups still serve the old
//! numbers.

use chrono::{DateTime, Utc};
use sea_orm::DatabaseConnection;

use crate::common::aggregates::{self, Window};
use crate::error::AppResult;

/// Refresh every rollup from `since` (bucket-floored), or over the rolling per-view defaults when
/// no instant is given.
pub async fn refresh_continuous_aggregates(
    db: &DatabaseConnection,
    since: Option<DateTime<Utc>>,
) -> AppResult<()> {
    aggregates::refresh(db, since.map_or(Window::Recent, Window::Since)).await
}

/// Refresh every rollup over the whole history. The repair backstop; expensive.
pub async fn refresh_continuous_aggregates_full(db: &DatabaseConnection) -> AppResult<()> {
    aggregates::refresh(db, Window::Full).await
}
