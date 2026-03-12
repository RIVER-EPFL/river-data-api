use async_trait::async_trait;
use sea_orm::DatabaseConnection;

use crate::error::AppResult;

/// Trait defining the interface for data connectors (Vaisala, Astrocast, CNET, etc.).
///
/// Each connector is responsible for discovering entities (projects, sites, parameters)
/// and syncing readings from its data source into the unified river-data schema.
///
/// Vaisala does not implement this trait yet — it documents the target interface
/// for future multi-connector support.
#[async_trait]
pub trait DataConnector: Send + Sync {
    /// Human-readable name for this connector (e.g., "vaisala", "astrocast").
    fn name(&self) -> &str;

    /// Discover projects, sites, and parameters from the external source.
    /// Creates any missing entities in the database.
    async fn discover(&self, db: &DatabaseConnection) -> AppResult<()>;

    /// Sync readings from the external source.
    /// If `force_full` is true, ignores incremental state and fetches full history.
    async fn sync_readings(&self, db: &DatabaseConnection, force_full: bool) -> AppResult<()>;
}
