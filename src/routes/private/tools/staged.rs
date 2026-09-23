//! The values an operator has typed at a visit and not saved, and what a calculation reads them
//! as.
//!
//! A preview of the calculation chain runs on the grid as it stands in front of the operator, not
//! on the store. The overlay that expresses that difference is built here, server-side and once:
//! a staged number is corrected by the same instrument, calibration and standard curve the save
//! would apply, so a previewed value and the value the save stores are the same arithmetic on the
//! same inputs. A cell nobody touched is absent from the overlay and reads from the store; a cell
//! the operator emptied is present with no value, and reads as the withdrawal a save would record
//! (Q227).

use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::routes::private::data_streams::models as data_streams;
use crate::routes::private::readings::service::{CurveClaim, admit_standard_curves};
use crate::routes::private::sensor_calibrations::resolver;
use crate::routes::private::sensor_calibrations::service::{Curve, apply_curves};
use crate::routes::private::site_parameters::models as site_parameters;

/// A grab is spot by construction, so a staged cell is admitted under the spot arm.
const STAGED_MEASUREMENT_TYPE: &str = river_data_core::models::MeasurementType::Spot.as_str();

/// One cell of a visit as the operator has left it, before any save.
///
/// A cell the request does not carry is a cell nobody touched: the calculation reads whatever the
/// store holds there. A cell carrying `value: null` is the operator emptying it, which a save
/// records as a withdrawal, so the calculation reads the visit without it.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct StagedCell {
    pub parameter_id: Uuid,
    /// Position in the family. A single measurement is index 0.
    pub replicate_index: i16,
    /// The number as typed, uncorrected. `null` empties the cell.
    #[schema(required)]
    pub value: Option<f64>,
    /// The standard curve the operator picked for this cell, where the slot takes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub standard_curve_id: Option<Uuid>,
}

/// The visit's unsaved cells, resolved to the numbers a calculation reads.
#[derive(Debug, Clone, Default)]
pub struct StagedVisit {
    /// The served number each staged cell contributes, `None` where the cell was emptied. A key
    /// absent here is a cell nobody touched.
    cells: BTreeMap<(Uuid, i16), Option<f64>>,
    /// Parameters whose stored family this pass has retracted whole, because a calculation
    /// cleared its output: a later reader sees the slot empty, as the save's withdrawal leaves it.
    retracted: HashSet<Uuid>,
    /// The grab stream each staged parameter would write to, where the slot already has one. A
    /// slot that has never held a grab has none, and a preview does not mint one.
    streams: HashMap<Uuid, Uuid>,
}

impl StagedVisit {
    /// Resolve what the operator typed into the numbers a calculation reads: the slot's declared
    /// instrument corrects the value, then the curve the operator picked, in the one order
    /// [`apply_curves`] defines. A cell naming a parameter the site does not carry is refused
    /// here, the rule a hand save is held to.
    ///
    /// # Errors
    /// When a cell names a parameter the site holds no slot for, or a curve the slot may not use.
    pub async fn resolve(
        db: &DatabaseConnection,
        site_id: Uuid,
        collected_at: DateTime<Utc>,
        cells: &[StagedCell],
    ) -> AppResult<Self> {
        let mut staged = Self {
            streams: grab_streams(db, site_id).await?,
            ..Self::default()
        };
        if cells.is_empty() {
            return Ok(staged);
        }
        let parameter_ids: Vec<Uuid> = cells.iter().map(|c| c.parameter_id).collect();
        let slots = site_parameters::Entity::find()
            .filter(site_parameters::Column::SiteId.eq(site_id))
            .filter(site_parameters::Column::ParameterId.is_in(parameter_ids.clone()))
            .all(db)
            .await?;
        let declared: HashSet<Uuid> = slots.iter().map(|sp| sp.parameter_id).collect();
        if let Some(cell) = cells.iter().find(|c| !declared.contains(&c.parameter_id)) {
            return Err(AppError::BadRequest(format!(
                "Parameter {} is not configured for site {site_id}, so nothing can be staged \
                 against it",
                cell.parameter_id
            )));
        }
        let instruments: HashMap<Uuid, Uuid> = slots
            .iter()
            .filter_map(|sp| sp.instrument_sensor_id.map(|sid| (sp.parameter_id, sid)))
            .collect();

        let requests: Vec<(Uuid, Option<Uuid>, DateTime<Utc>)> = cells
            .iter()
            .filter(|c| c.value.is_some())
            .filter_map(|c| {
                instruments
                    .get(&c.parameter_id)
                    .map(|sid| (*sid, Some(c.parameter_id), collected_at))
            })
            .collect();
        let base_curves = resolver::resolve_many(db, &requests).await?;

        let claims: Vec<CurveClaim<'_>> = cells
            .iter()
            .filter_map(|c| {
                c.standard_curve_id.map(|id| CurveClaim {
                    standard_curve_id: id,
                    sensor_id: instruments.get(&c.parameter_id).copied(),
                    measurement_type: STAGED_MEASUREMENT_TYPE,
                })
            })
            .collect();
        let standard_curves = admit_standard_curves(db, &claims).await?;

        for cell in cells {
            let served = cell.value.map(|raw| {
                let base = instruments
                    .get(&cell.parameter_id)
                    .and_then(|sid| base_curves.get(&(*sid, Some(cell.parameter_id), collected_at)))
                    .copied();
                let standard = cell.standard_curve_id.map(|id| {
                    let c = &standard_curves[&id];
                    Curve {
                        id: c.id,
                        slope: c.slope,
                        intercept: c.intercept,
                    }
                });
                apply_curves(raw, base, standard)
            });
            staged
                .cells
                .insert((cell.parameter_id, cell.replicate_index), served);
        }
        Ok(staged)
    }

    /// Whether this pass has moved the family at `parameter_id`, so its statistics are recomputed
    /// here rather than read from the stored `samples` row.
    #[must_use]
    pub fn touches(&self, parameter_id: Uuid) -> bool {
        self.retracted.contains(&parameter_id) || self.cells.keys().any(|(p, _)| *p == parameter_id)
    }

    /// Whether a calculation cleared this parameter's output at the visit, retracting whatever the
    /// store holds there.
    #[must_use]
    pub fn is_retracted(&self, parameter_id: Uuid) -> bool {
        self.retracted.contains(&parameter_id)
    }

    /// The staged cells of one parameter, lowest index first. `None` is a cell emptied.
    pub fn cells_of(&self, parameter_id: Uuid) -> impl Iterator<Item = (i16, Option<f64>)> + '_ {
        self.cells
            .range((parameter_id, i16::MIN)..=(parameter_id, i16::MAX))
            .map(|((_, index), value)| (*index, *value))
    }

    /// The grab stream a staged cell of this parameter stands at, where the slot has one.
    #[must_use]
    pub fn stream_of(&self, parameter_id: Uuid) -> Option<Uuid> {
        self.streams.get(&parameter_id).copied()
    }

    /// Take a calculation's output into the overlay, so the tools that read it downstream see what
    /// this pass produced rather than what the store still holds. A per-replicate output keeps its
    /// index, and an index it produced nothing at stays a gap.
    pub fn take_output(&mut self, parameter_id: Uuid, values: &[(i16, f64)]) {
        self.retracted.insert(parameter_id);
        for (index, value) in values {
            self.cells.insert((parameter_id, *index), Some(*value));
        }
    }

    /// Take a calculation's cleared output into the overlay: the slot reads empty downstream, as
    /// the save's withdrawal leaves it.
    pub fn retract(&mut self, parameter_id: Uuid) {
        self.retracted.insert(parameter_id);
        self.cells.retain(|(p, _), _| *p != parameter_id);
    }
}

/// The grab stream of every parameter at a site, looked up and never created: a preview writes
/// nothing, and a slot that has never held a grab has no stream to name. The whole site, because
/// a calculation's output lands on a slot the request never named.
async fn grab_streams(db: &DatabaseConnection, site_id: Uuid) -> AppResult<HashMap<Uuid, Uuid>> {
    let prefix = format!("{site_id}:");
    let rows = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq("grab_sample"))
        .filter(data_streams::Column::SourceKey.starts_with(&prefix))
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|s| {
            let parameter_id = s.source_key.strip_prefix(&prefix)?.parse().ok()?;
            Some((parameter_id, s.id))
        })
        .collect())
}

#[cfg(test)]
#[path = "tests/staged.rs"]
mod tests;
