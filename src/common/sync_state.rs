//! The continuous-aggregate refresh as the sync, calibration and job paths call it.
//!
//! The window arithmetic, the view list and the error handling all live in
//! [`crate::common::aggregates`]; this module is only the call shape those sites use. It is
//! fallible: a refresh that could not run has to reach the caller that asked for it, or a tracked
//! job reports `completed` while the rollups still serve the old numbers.

use chrono::{DateTime, Utc};
use sea_orm::DatabaseConnection;

use crate::common::aggregates::{self, Window};
use crate::error::AppResult;

/// Refresh every rollup from `since` (bucket-floored) to now, so a change reaches the rollups
/// before the policy's next tick would carry it.
pub async fn refresh_continuous_aggregates(
    db: &DatabaseConnection,
    since: DateTime<Utc>,
) -> AppResult<()> {
    aggregates::refresh(db, Window::Since(since)).await
}
