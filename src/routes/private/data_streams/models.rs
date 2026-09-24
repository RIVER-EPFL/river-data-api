//! The stream entity, the replicate declaration it carries, and the request and response
//! shapes of the stream routes.

use chrono::Utc;
use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::service::DataStreamOperations;
use crate::common::bulk_write::TouchedRange;
use crate::error::{AppError, AppResult};
use crate::routes::private::collection_events::flows::TouchedEvent;

#[derive(
    Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "data_streams")]
#[crudcrate(
    api_struct = "DataStream",
    name_singular = "data_stream",
    name_plural = "data_streams",
    generate_router,
    operations = DataStreamOperations,
    upsert_key(source_system, source_key)
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable, sortable, exclude(update))]
    pub source_system: String,
    #[crudcrate(filterable, fulltext, sortable, exclude(update))]
    pub source_key: String,
    #[crudcrate(fulltext, sortable)]
    pub source_name: Option<String>,
    #[crudcrate(fulltext)]
    pub source_path: Option<String>,
    // The replicate column-to-index assignments live here and are append-only: register, pair and
    // retag are the only writers. A CRUD PATCH must not be able to re-point or erase them.
    #[sea_orm(column_type = "JsonBinary")]
    #[crudcrate(exclude(update))]
    pub metadata: serde_json::Value,
    #[crudcrate(filterable)]
    pub site_parameter_id: Option<Uuid>,
    #[crudcrate(filterable)]
    pub sensor_id: Option<Uuid>,
    /// Stream-level default for readings.measurement_type ('continuous' | 'spot' | 'derived').
    /// NULL defers to the owning sensor's data_frequency, then falls back to 'continuous'.
    #[crudcrate(filterable)]
    pub measurement_type: Option<String>,
    #[crudcrate(filterable, on_create = true)]
    pub is_active: bool,
    #[crudcrate(sortable, exclude(create, update))]
    pub discovered_at: DateTimeWithTimeZone,
    #[crudcrate(sortable)]
    pub paired_at: Option<DateTimeWithTimeZone>,
    #[crudcrate(sortable, exclude(update))]
    pub last_data_time: Option<DateTimeWithTimeZone>,
    /// Content digest of the last cleanly-applied windowed pass, as the sync client claimed it.
    #[crudcrate(exclude(create, update))]
    pub last_window_digest: Option<String>,
    #[crudcrate(filterable, exclude(create, update))]
    pub pairing_plan_id: Option<Uuid>,
    #[crudcrate(exclude(create, update))]
    pub created_at: DateTimeWithTimeZone,
    #[crudcrate(exclude(create, update), on_update = DateTimeWithTimeZone::from(chrono::Utc::now()))]
    pub updated_at: DateTimeWithTimeZone,
    /// The authoritative replicate column-to-index mapping, ordered by index, when this stream
    /// declares a replicate family. Sync services assign each value's `replicate_index` from it;
    /// a retired entry keeps its index reserved for the readings already stored under it. Derived
    /// from `metadata.replicates`, never written on its own.
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub replicates: Option<Vec<ColumnAssignment>>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::site_parameters::Entity",
        from = "Column::SiteParameterId",
        to = "crate::routes::private::site_parameters::Column::Id"
    )]
    SiteParameter,
    #[sea_orm(
        belongs_to = "crate::routes::private::sensors::Entity",
        from = "Column::SensorId",
        to = "crate::routes::private::sensors::Column::Id"
    )]
    Sensor,
    #[sea_orm(
        belongs_to = "crate::routes::private::data_streams::pairing_plans::Entity",
        from = "Column::PairingPlanId",
        to = "crate::routes::private::data_streams::pairing_plans::Column::Id"
    )]
    PairingPlan,
}

impl Related<crate::routes::private::site_parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SiteParameter.def()
    }
}

impl Related<crate::routes::private::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensor.def()
    }
}

impl Related<crate::routes::private::data_streams::pairing_plans::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::PairingPlan.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

/// The metadata key the spec is stored under.
pub const METADATA_KEY: &str = "replicates";

// --- The replicate family ---
//
// The typed declaration that one stream is fed by several portal columns as replicates of one
// logical parameter. Registered by a sync service on `/streams/register`, persisted under
// `data_streams.metadata["replicates"]`, and read back by the UI and the reconciliation job.
// Membership is validated and refused here, never inferred from column-name shape: the portals'
// naming carries five inconsistent suffix dialects and several traps (`S275_295` is a wavelength
// range, `WTW_pH_1` a legacy marker), so it only ever comes from the source's own calculation
// registry, carried in this spec.

/// One source column's permanent replicate index, and the declaration a sync service registers.
/// Both are declared in `river-data-core`: the mapping travels back on the register response and
/// decides which index a reading is stored under, so neither side may author it alone.
///
/// The mapping is append-only: a column keeps its index for the life of the stream, a new column
/// appends after the highest index ever assigned, and a column the source stops sending is retired
/// with its index never reused.
pub use river_data_core::models::ColumnAssignment;

/// A stream's replicate family as it is stored: the source's declaration, plus the column-to-index
/// mapping the register path pins. The caller declares columns and never authors indexes, so the
/// two halves have separate authors and only this stored form carries both. It is not a wire
/// shape: a registration carries the declaration alone, and this is what stream metadata holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicateSpec {
    #[serde(flatten)]
    pub declared: river_data_core::models::ReplicateSpec,
    /// The authoritative column-to-index mapping, authored by the register path (never by the
    /// caller) via [`pin_assignments`]. Readings carry these indexes for life, so re-registration
    /// preserves them: see [`ColumnAssignment`].
    #[serde(default)]
    pub assignments: Vec<ColumnAssignment>,
}

impl ReplicateSpec {
    /// Store the spec under [`METADATA_KEY`] in a stream's metadata object.
    pub fn embed(&self, metadata: &mut serde_json::Value) -> AppResult<()> {
        let spec = serde_json::to_value(self)
            .map_err(|e| AppError::Internal(format!("replicate spec serialisation: {e}")))?;
        match metadata {
            serde_json::Value::Object(map) => {
                map.insert(METADATA_KEY.to_string(), spec);
                Ok(())
            }
            serde_json::Value::Null => {
                *metadata = serde_json::json!({ METADATA_KEY: spec });
                Ok(())
            }
            _ => Err(AppError::BadRequest(
                "stream metadata must be an object to carry a replicate spec".to_string(),
            )),
        }
    }

    /// Parse the spec out of a stream's metadata, if one was registered.
    #[must_use]
    pub fn from_metadata(metadata: &serde_json::Value) -> Option<Self> {
        serde_json::from_value(metadata.get(METADATA_KEY)?.clone()).ok()
    }

    /// The authoritative mapping, ordered by index. A spec stored before pinning derives it from
    /// its column positions, which were the indexes at the time; core resolves both sides the same
    /// way, so a sync service never invents an index this would not.
    #[must_use]
    pub fn column_assignments(&self) -> Vec<ColumnAssignment> {
        ColumnAssignment::resolve(self.assignments.clone(), &self.declared.source_columns)
    }
}

/// What one backfill moved, and the visits whose calculations it owes a run.
pub struct Backfilled {
    pub readings: u64,
    pub touched_events: Vec<TouchedEvent>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StreamStatsResponse {
    pub stream_id: Uuid,
    pub reading_count: i64,
    /// Rows stamped withdrawn by windowed reconciliation (included in `reading_count`).
    pub withdrawn_count: i64,
    #[schema(required)]
    pub min_time: Option<chrono::DateTime<Utc>>,
    #[schema(required)]
    pub max_time: Option<chrono::DateTime<Utc>>,
    #[schema(required)]
    pub latest_value: Option<f64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PreviewReplicate {
    pub replicate_index: i16,
    /// The source column this index is pinned to, when the stream declares a replicate spec.
    #[schema(required)]
    pub column: Option<String>,
    #[schema(required)]
    pub value: Option<f64>,
    pub is_flagged: bool,
    pub withdrawn: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PreviewInstant {
    pub time: chrono::DateTime<Utc>,
    pub replicates: Vec<PreviewReplicate>,
    /// Recomputed here from the served replicates, which is what `samples` holds for a paired
    /// stream. Shown so the review can see the statistics the pairing will produce before it
    /// produces them.
    #[schema(required)]
    pub mean: Option<f64>,
    #[schema(required)]
    pub sd: Option<f64>,
    pub n: usize,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StreamPreviewResponse {
    pub stream_id: Uuid,
    pub source_key: String,
    pub instants: Vec<PreviewInstant>,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct ReceiptsQuery {
    /// 1-based page, default 1.
    #[serde(default)]
    pub page: Option<u64>,
    /// Rows per page, default 50, max 200.
    #[serde(default)]
    pub page_size: Option<u64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReceiptRow {
    pub id: Uuid,
    pub at: chrono::DateTime<Utc>,
    #[schema(required)]
    pub window_from: Option<chrono::DateTime<Utc>>,
    #[schema(required)]
    pub window_to: Option<chrono::DateTime<Utc>>,
    pub submitted: i32,
    pub new_rows: i32,
    pub changed: i32,
    pub unchanged: i32,
    pub retained: i32,
    pub rejected_total: i32,
    pub dropped: i32,
    pub withdrawn: i32,
    pub braked: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReceiptsResponse {
    pub stream_id: Uuid,
    pub total: u64,
    pub receipts: Vec<ReceiptRow>,
}

/// A stream registration, as a sync service sends it. The field list is
/// `river_data_core::models::RegisterStreamRequest`, which the clients build from, so a field the
/// sender gains cannot be dropped here in silence. `metadata` has always been optional on this
/// route and core declares it required, so an omitted object is filled in before the body is read.
#[derive(Debug, ToSchema)]
#[schema(value_type = river_data_core::models::RegisterStreamRequest)]
pub struct RegisterStreamRequest(pub river_data_core::models::RegisterStreamRequest);

impl<'de> Deserialize<'de> for RegisterStreamRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        crate::routes::private::wire::defaulted(deserializer, &[("metadata", default_metadata())])
            .map(Self)
    }
}

fn default_metadata() -> serde_json::Value {
    serde_json::json!({})
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PairStreamRequest {
    pub site_parameter_id: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PairStreamResponse {
    pub stream: DataStream,
    pub backfilled: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UnpairStreamResponse {
    pub stream: DataStream,
    pub cleared: u64,
}

#[derive(Debug, serde::Deserialize, ToSchema)]
pub struct RetagStreamsRequest {
    /// Explicit streams to classify. Combined with `source_system` when both are given.
    #[serde(default)]
    pub stream_ids: Vec<Uuid>,
    /// Classify every stream of a source system (e.g. 'metalp', 'nomis').
    #[serde(default)]
    pub source_system: Option<String>,
    /// 'continuous' | 'spot' | 'derived', or 'declared' to keep each stream's own classification
    /// and align its readings with it (mixed source systems such as cnet).
    pub measurement_type: String,
    /// Also retag the streams' existing readings and refresh aggregates (tracked job).
    #[serde(default)]
    pub retag_existing: bool,
}

#[derive(Debug, serde::Serialize, ToSchema)]
pub struct RetagStreamsResponse {
    pub streams_updated: u64,
    pub measurement_type: String,
    /// The tracked `measurement_retag` job, when `retag_existing` was requested.
    #[schema(required)]
    pub job_id: Option<Uuid>,
}

// --- The (site, parameter) slot: one declaration, two directions ---

/// What a dying slot does with one table's rows.
#[derive(Debug, Clone, Copy)]
pub enum Release {
    /// The measurement outlives the slot: null these columns and keep the row.
    Unattribute(&'static [&'static str]),
    /// The row only describes a group of readings: delete it once none references it.
    DeleteWhenOrphaned,
    /// Describes the site and the parameter rather than the slot's data, so it outlives the slot.
    Retain,
}

/// The tables a slot owns rows in, named through their entities so a statement over one cannot
/// spell a table or a column the entity does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotRows {
    Readings,
    StatusEvents,
    Samples,
    Annotations,
}

impl SlotRows {
    /// The table, as the builder names it.
    #[must_use]
    pub fn table_ref(self) -> sea_orm::sea_query::TableRef {
        use sea_orm::sea_query::IntoTableRef;
        match self {
            Self::Readings => crate::routes::private::readings::models::Entity.into_table_ref(),
            Self::StatusEvents => {
                crate::routes::private::readings::status_events::models::Entity.into_table_ref()
            }
            Self::Samples => {
                crate::routes::private::readings::samples::models::Entity.into_table_ref()
            }
            Self::Annotations => crate::routes::private::annotations::Entity.into_table_ref(),
        }
    }

    /// The table's own name, for a message naming which one a collision was found in.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Readings => "readings",
            Self::StatusEvents => "status_events",
            Self::Samples => "samples",
            Self::Annotations => "annotations",
        }
    }
}

/// A table addressed by the (site, parameter) slot rather than by the stream that wrote its rows.
#[derive(Debug, Clone, Copy)]
pub struct SlotTable {
    pub rows: SlotRows,
    pub release: Release,
    /// Rows carry a `time` column, so a mutation can report the span it touched.
    pub timed: bool,
    /// Rows feed the continuous aggregates, so a mutation here decides the refresh window.
    pub feeds_rollups: bool,
    /// Column completing a `(site_id, parameter_id, ...)` unique constraint, so moving these rows
    /// onto a surviving slot can collide.
    pub unique_with: Option<&'static str>,
}

/// Every table keyed by `(site_id, parameter_id)`.
///
/// A merge re-points all of them onto the survivor ([`move_slot_rows`]); a slot that dies releases
/// each according to its `release` ([`retire_slot`]). Both directions read this one list, which is
/// what stops a merge stranding rows on a deleted parameter and a slot delete abandoning them.
pub const SLOT_TABLES: [SlotTable; 4] = [
    SlotTable {
        rows: SlotRows::Readings,
        release: Release::Unattribute(&[
            "site_id",
            "parameter_id",
            "sample_id",
            "collection_event_id",
        ]),
        timed: true,
        feeds_rollups: true,
        unique_with: None,
    },
    SlotTable {
        rows: SlotRows::StatusEvents,
        release: Release::Unattribute(&["site_id", "parameter_id"]),
        timed: true,
        feeds_rollups: false,
        unique_with: None,
    },
    SlotTable {
        rows: SlotRows::Samples,
        release: Release::DeleteWhenOrphaned,
        timed: false,
        feeds_rollups: false,
        unique_with: Some("collected_at"),
    },
    SlotTable {
        rows: SlotRows::Annotations,
        release: Release::Retain,
        timed: false,
        feeds_rollups: false,
        unique_with: None,
    },
];

/// Which rows a slot teardown covers.
#[derive(Debug, Clone, Copy)]
pub enum SlotScope {
    /// One stream's rows. The slot itself survives; this stream stops feeding it (unpair).
    Stream(Uuid),
    /// Everything the slot owns, whatever wrote it, plus the streams pointing at it. The slot is
    /// going away (site_parameter delete).
    SiteParameter(Uuid),
}

impl SlotScope {
    /// The streams the teardown releases from the slot.
    pub fn streams(self) -> sea_orm::Condition {
        match self {
            Self::Stream(id) => sea_orm::Condition::all().add(Column::Id.eq(id)),
            Self::SiteParameter(id) => {
                sea_orm::Condition::all().add(Column::SiteParameterId.eq(id))
            }
        }
    }

    /// The `reason` a released reading's `attribution` decision records.
    #[must_use]
    pub fn release_reason(self) -> &'static str {
        match self {
            Self::Stream(_) => "unpaired",
            Self::SiteParameter(_) => "slot deleted",
        }
    }
}

/// Which sites a slot move covers.
#[derive(Debug, Clone, Copy)]
pub enum MoveScope {
    /// One site's rows, for a site-level merge.
    Site(Uuid),
    /// Every site carrying the source parameter, for a catalog-level merge.
    EverySite,
}

/// Rows a slot move carried, and the span the moved readings cover.
#[derive(Debug, Clone, Default)]
pub struct SlotMove {
    pub readings: u64,
    pub status_events: u64,
    /// Feeds the caller's post-commit rollup refresh; the rollups group by `parameter_id`, so both
    /// the source's and the survivor's buckets are recomputed by the same window.
    pub touched: TouchedRange,
    /// Visits whose inputs moved, for the post-commit calculation hook.
    pub touched_events: Vec<crate::routes::private::collection_events::flows::TouchedEvent>,
    /// The readings moved per site, at the absorbed and the surviving parameter, for the caller's
    /// post-commit announcement.
    pub changed: crate::common::SlotTally,
}

/// One committed windowed ingest pass.
///
/// The row is the account of what the pass did with what the source submitted, and the table's
/// `receipt_arithmetic_closes` CHECK is what makes a write path that cannot account for a
/// submitted row unable to commit. Named fields on the writer are the other half of that: the
/// counts are five separate `integer` columns, and a positional bind list cannot tell them apart.
///
/// Read-only as an entity (`routes(read)`): the one writer is the ingest pass itself and the one
/// deleter is the janitor's age prune.
pub mod receipts {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(
        Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, EntityToModels,
    )]
    #[sea_orm(table_name = "ingest_receipts")]
    #[crudcrate(
        api_struct = "IngestReceipt",
        name_singular = "ingest_receipt",
        name_plural = "ingest_receipts",
        generate_router,
        routes(read)
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable, sortable, exclude(update, create))]
        pub stream_id: Uuid,
        #[crudcrate(sortable, exclude(update, create))]
        pub at: DateTimeWithTimeZone,
        #[crudcrate(sortable, exclude(update, create))]
        pub window_from: Option<DateTimeWithTimeZone>,
        #[crudcrate(sortable, exclude(update, create))]
        pub window_to: Option<DateTimeWithTimeZone>,
        #[crudcrate(exclude(update, create))]
        pub submitted: i32,
        #[crudcrate(exclude(update, create))]
        pub new_rows: i32,
        #[crudcrate(exclude(update, create))]
        pub changed: i32,
        #[crudcrate(exclude(update, create))]
        pub unchanged: i32,
        #[crudcrate(exclude(update, create))]
        pub retained: i32,
        #[crudcrate(exclude(update, create))]
        pub rejected_total: i32,
        #[crudcrate(exclude(update, create))]
        pub rejected: serde_json::Value,
        #[crudcrate(exclude(update, create))]
        pub dropped: i32,
        #[crudcrate(exclude(update, create))]
        pub withdrawn: i32,
        #[crudcrate(exclude(update, create))]
        pub changed_keys: Option<serde_json::Value>,
        #[crudcrate(filterable, exclude(update, create))]
        pub braked: bool,
        #[crudcrate(exclude(update, create))]
        pub brake_threshold: Option<f32>,
        #[crudcrate(exclude(update, create))]
        pub proposed: i32,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}
