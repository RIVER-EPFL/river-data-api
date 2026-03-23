pub mod nomis;
pub mod rshiny;

use chrono::{DateTime, Utc};
use sqlx::MySqlPool;
use uuid::Uuid;

use crate::error::SyncError;

/// Describes a data stream to be registered with river-data.
#[derive(Debug, Clone)]
pub struct StreamDescriptor {
    /// Unique key within source_system (e.g., "VAD:WTW_DO_mgL_1")
    pub source_key: String,
    /// Human-readable name (e.g., "Dissolved Oxygen - Field [mg/L]")
    pub source_name: String,
    /// Hierarchy path (e.g., "cnet/VAD/WTW_DO_mgL_1")
    pub source_path: String,
    /// Metadata JSON
    pub metadata: serde_json::Value,
}

/// Request to fetch readings for a specific stream since a given time.
#[derive(Debug, Clone)]
pub struct StreamFetchRequest {
    pub source_key: String,
    pub stream_id: Uuid,
    pub since: Option<DateTime<Utc>>,
}

/// Readings fetched for a single stream.
#[derive(Debug)]
pub struct StreamReadings {
    pub source_key: String,
    pub stream_id: Uuid,
    pub readings: Vec<ReadingValue>,
}

/// A single reading value with timestamp and optional replicate index.
#[derive(Debug, Clone)]
pub struct ReadingValue {
    pub time: DateTime<Utc>,
    pub value: f64,
    pub replicate_index: i32,
}

/// Portal-specific extraction logic.
#[async_trait::async_trait]
pub trait PortalBackend: Send + Sync + 'static {
    /// The source_system string for river-data (e.g., "cnet", "metalp", "nomis").
    fn source_system(&self) -> &str;

    /// Discover all streams that should be registered.
    async fn discover_stream_descriptors(
        &self,
        pool: &MySqlPool,
    ) -> Result<Vec<StreamDescriptor>, SyncError>;

    /// Fetch readings for the given streams since their last sync time.
    async fn fetch_readings(
        &self,
        pool: &MySqlPool,
        streams: &[StreamFetchRequest],
    ) -> Result<Vec<StreamReadings>, SyncError>;
}
