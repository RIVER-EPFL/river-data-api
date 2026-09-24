//! The portal function a synced column was computed by, read off its stream's descriptor, with
//! each column it read opened as a record where the same site holds it at the same instant.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, JoinType, QueryFilter, QuerySelect, RelationTrait,
};
use uuid::Uuid;

use super::models::{PortalCalculation, PortalInput, SlotRef};
use crate::error::AppResult;
use crate::routes::private::data_streams;
use crate::routes::private::readings;
use crate::routes::private::site_parameters;
use crate::routes::private::sync::service::plan_calculation;

/// The portal calculation a stream declares, its inputs opened at `site_id` and `time`. `None`
/// for a column the source stores as entered.
pub async fn of_stream<C: ConnectionTrait>(
    db: &C,
    stream: &data_streams::Model,
    site_id: Option<Uuid>,
    time: DateTime<Utc>,
) -> AppResult<Option<PortalCalculation>> {
    let Some(declared) = plan_calculation(&stream.source_system, &stream.metadata) else {
        return Ok(None);
    };
    let streams = match site_id {
        Some(site_id) => {
            let candidates = paired_streams_at(db, &stream.source_system, site_id).await?;
            streams_for_columns(&candidates, &declared.inputs)
        }
        None => HashMap::new(),
    };
    let points = points_at(db, &streams, time).await?;
    Ok(Some(PortalCalculation {
        function: declared.function,
        inputs: declared
            .inputs
            .into_iter()
            .map(|column| PortalInput {
                stream_id: streams.get(&column).copied(),
                point: points.get(&column).cloned(),
                column,
            })
            .collect(),
    }))
}

/// The column a stream carries, as its descriptor names it.
fn column_of(metadata: &serde_json::Value) -> Option<&str> {
    metadata.get("parameter")?.get("column_name")?.as_str()
}

/// Each input column matched to the stream carrying it among `candidates` (stream id, metadata).
/// A column no candidate carries is left out.
pub(super) fn streams_for_columns(
    candidates: &[(Uuid, serde_json::Value)],
    columns: &[String],
) -> HashMap<String, Uuid> {
    candidates
        .iter()
        .filter_map(|(id, metadata)| {
            let column = column_of(metadata)?;
            columns
                .iter()
                .any(|c| c == column)
                .then(|| (column.to_string(), *id))
        })
        .collect()
}

/// The streams of one source system paired to a slot at the site, with their descriptors.
async fn paired_streams_at<C: ConnectionTrait>(
    db: &C,
    source_system: &str,
    site_id: Uuid,
) -> AppResult<Vec<(Uuid, serde_json::Value)>> {
    Ok(data_streams::Entity::find()
        .select_only()
        .column(data_streams::Column::Id)
        .column(data_streams::Column::Metadata)
        .join(
            JoinType::InnerJoin,
            data_streams::Relation::SiteParameter.def(),
        )
        .filter(data_streams::Column::SourceSystem.eq(source_system))
        .filter(site_parameters::Column::SiteId.eq(site_id))
        .into_tuple()
        .all(db)
        .await?)
}

/// A reading's stream, site, slot and cadence, the parts a record link is built from.
type PointRow = (Uuid, Option<Uuid>, Option<Uuid>, Option<String>);

/// The record each column's stream holds at `time`, keyed by column.
async fn points_at<C: ConnectionTrait>(
    db: &C,
    streams: &HashMap<String, Uuid>,
    time: DateTime<Utc>,
) -> AppResult<HashMap<String, SlotRef>> {
    if streams.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<PointRow> = readings::Entity::find()
        .select_only()
        .column(readings::Column::StreamId)
        .column(readings::Column::SiteId)
        .column(data_streams::Column::SiteParameterId)
        .column(readings::Column::MeasurementType)
        .join(JoinType::InnerJoin, readings::Relation::DataStream.def())
        .filter(readings::Column::StreamId.is_in(streams.values().copied()))
        .filter(readings::Column::Time.eq(time))
        .distinct()
        .into_tuple()
        .all(db)
        .await?;
    let by_stream: HashMap<Uuid, SlotRef> = rows
        .into_iter()
        .filter_map(
            |(stream_id, site_id, site_parameter_id, measurement_type)| {
                Some((
                    stream_id,
                    SlotRef {
                        site_id: site_id?,
                        site_parameter_id: site_parameter_id?,
                        time,
                        measurement_type: match measurement_type.as_deref() {
                            Some("spot") => "spot".to_string(),
                            _ => "continuous".to_string(),
                        },
                    },
                ))
            },
        )
        .collect();
    Ok(streams
        .iter()
        .filter_map(|(column, id)| Some((column.clone(), by_stream.get(id)?.clone())))
        .collect())
}

#[cfg(test)]
#[path = "tests/portal_calculation.rs"]
mod tests;
