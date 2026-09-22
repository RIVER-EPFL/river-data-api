//! A computed reading's inputs as it consumed them, beside what their sources hold now (Q215).
//!
//! The capture side writes a [`ConsumedInput`] per input at the read: its binding, the value, and
//! the revision of every row behind it. This module is the read side. It resolves each captured
//! revision against the source's current one and marks the input `changed`, `unchanged` or
//! `unknown`, and it names the point record each consumed reading opens.
//!
//! Nothing here recomputes a value. A statistic over several readings is not re-derived from the
//! members' current values, because a number the calculation never saw and the store does not
//! hold would read as the source's own.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sea_orm::sea_query::{Alias, Expr, Func, JoinType, Query};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, ExprTrait, FromQueryResult, QueryFilter};
use uuid::Uuid;

use super::models::{ConsumedInput, ConsumedMemberRef, ConsumedRef, SlotRef};
use crate::error::AppResult;
use crate::routes::private::change_audit::service::entity_revisions;
use crate::routes::private::constants;
use crate::routes::private::data_streams;
use crate::routes::private::derived_parameters::models::definition as calculation_formulas;
use crate::routes::private::readings;
use crate::routes::private::sites;
use crate::routes::private::standard_curves;

const CHANGED: &str = "changed";
const UNCHANGED: &str = "unchanged";
const UNKNOWN: &str = "unknown";

/// What a source stands at now, against the revision a calculation consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Standing {
    /// The source answers, at this revision; `None` is its arrival state, before any decision.
    At(Option<i64>),
    /// Nothing answers at the key or the subject any more.
    Gone,
    /// The input named no source: a typed value, or coefficients entered in the catalog's place.
    Unsourced,
}

/// The mark one source carries. Revision equality decides it, and an input with no source to
/// compare against is `unknown` rather than assumed to have held still.
fn mark(consumed: Option<i64>, current: Standing) -> &'static str {
    match current {
        Standing::Unsourced => UNKNOWN,
        Standing::Gone => CHANGED,
        Standing::At(now) if now == consumed => UNCHANGED,
        Standing::At(_) => CHANGED,
    }
}

/// The mark an input carries over the sources behind it: one moved source changes the input, and
/// a source nothing can be said about leaves it unknown.
fn combine(marks: &[&'static str]) -> &'static str {
    if marks.contains(&CHANGED) {
        CHANGED
    } else if marks.is_empty() || marks.contains(&UNKNOWN) {
        UNKNOWN
    } else {
        UNCHANGED
    }
}

/// One reading key as it stands now: its revision, the value it serves, and the slot its record
/// is read by.
#[derive(Debug, Clone)]
struct CurrentReading {
    revision: Option<i64>,
    value: Option<f64>,
    point: Option<SlotRef>,
}

/// The key of one consumed reading.
type ReadingKey = (Uuid, DateTime<Utc>, i16);

#[derive(FromQueryResult)]
struct CurrentRow {
    stream_id: Uuid,
    time: DateTime<Utc>,
    replicate_index: i16,
    value: Option<f64>,
    revision: Option<i64>,
    site_id: Option<Uuid>,
    site_parameter_id: Option<Uuid>,
    measurement_type: Option<String>,
}

/// Resolve every record's captured set against the sources it named, in one pass of lookups.
///
/// The lookups are batched across records because a visit's detail assembles one record per
/// parameter, and a query per input per record is what that turns into otherwise.
pub(super) async fn resolve_many<C: ConnectionTrait>(
    db: &C,
    sets: &HashMap<Uuid, Vec<ConsumedInput>>,
) -> AppResult<HashMap<Uuid, Vec<ConsumedRef>>> {
    let all: Vec<&ConsumedInput> = sets.values().flatten().collect();
    if all.is_empty() {
        return Ok(HashMap::new());
    }
    let keys: Vec<ReadingKey> = all
        .iter()
        .flat_map(|input| {
            input
                .members
                .iter()
                .map(|m| (m.stream_id, m.time, m.replicate_index))
        })
        .collect();
    let current = current_readings(db, &keys).await?;
    let subjects: Vec<String> = all.iter().filter_map(|i| i.subject.clone()).collect();
    let revisions = entity_revisions(db, &subjects).await?;
    let values = entity_values(db, &subjects).await?;

    Ok(sets
        .iter()
        .map(|(stream_id, set)| {
            let resolved = set
                .iter()
                .map(|input| resolve_one(input, &current, &revisions, &values))
                .collect();
            (*stream_id, resolved)
        })
        .collect())
}

fn resolve_one(
    input: &ConsumedInput,
    current: &HashMap<ReadingKey, CurrentReading>,
    revisions: &HashMap<String, i64>,
    values: &HashMap<String, EntityValue>,
) -> ConsumedRef {
    let members: Vec<ConsumedMemberRef> = input
        .members
        .iter()
        .map(|m| {
            let now = current.get(&(m.stream_id, m.time, m.replicate_index));
            let standing = now.map_or(Standing::Gone, |c| Standing::At(c.revision));
            ConsumedMemberRef {
                stream_id: m.stream_id,
                time: m.time,
                replicate_index: m.replicate_index,
                revision: m.revision,
                value: m.value,
                current_revision: now.and_then(|c| c.revision),
                current_value: now.and_then(|c| c.value),
                state: mark(m.revision, standing).to_string(),
                point: now.and_then(|c| c.point.clone()),
            }
        })
        .collect();
    let (current_revision, current_value, state) = match &input.subject {
        Some(subject) => {
            let revision = revisions.get(subject).copied();
            let standing = revision.map_or(Standing::Gone, |seq| Standing::At(Some(seq)));
            (
                revision,
                values.get(subject).and_then(|row| row.read(input)),
                mark(input.revision, standing).to_string(),
            )
        }
        None if members.is_empty() => (
            None,
            None,
            mark(input.revision, Standing::Unsourced).to_string(),
        ),
        None => {
            let marks: Vec<&'static str> = members.iter().map(|m| mark_of(&m.state)).collect();
            // A single reading is the input's own value; a statistic over several is not
            // recomputed, so only its members carry a current number.
            let value = (members.len() == 1)
                .then(|| members[0].current_value.map(|v| serde_json::json!(v)))
                .flatten();
            (None, value, combine(&marks).to_string())
        }
    };
    ConsumedRef {
        variable: input.variable.clone(),
        kind: input.kind.clone(),
        alignment: input.alignment.clone(),
        subject: input.subject.clone(),
        property: input.property.clone(),
        revision: input.revision,
        current_revision,
        value: input.value.clone(),
        current_value,
        state,
        members,
    }
}

/// The subject of an input that is the calculation's own text rather than something it read.
const FORMULA_SUBJECT: &str = "calculation_formula:";

/// The set a record reports over a key that has been computed more than once.
///
/// An input the calculation read from the catalog keeps the value and revision the first
/// computation read of it: editing a constant repairs the values it produced, and the record
/// still says which number produced them, marked `changed` against what the catalog holds
/// now (Q244). A member reading and a step formula follow the newest capture instead: correcting
/// a reading is the record following its source, and editing a step is the calculation itself
/// moving, which the new capture is the account of.
pub(super) fn as_first_read(
    newest: Vec<ConsumedInput>,
    first: &[ConsumedInput],
) -> Vec<ConsumedInput> {
    newest
        .into_iter()
        .map(|mut input| {
            let Some(subject) = input.subject.as_deref() else {
                return input;
            };
            if subject.starts_with(FORMULA_SUBJECT) {
                return input;
            }
            if let Some(origin) = first
                .iter()
                .find(|o| o.variable == input.variable && o.subject.as_deref() == Some(subject))
            {
                input.revision = origin.revision;
                input.value = origin.value.clone();
            }
            input
        })
        .collect()
}

/// A rendered mark read back as one of the three, so the aggregate cannot invent a fourth.
fn mark_of(state: &str) -> &'static str {
    match state {
        CHANGED => CHANGED,
        UNCHANGED => UNCHANGED,
        _ => UNKNOWN,
    }
}

/// Every named key as it stands now. The lookup is by stream and instant, both small sets for one
/// record, with the exact keys matched in memory.
async fn current_readings<C: ConnectionTrait>(
    db: &C,
    keys: &[ReadingKey],
) -> AppResult<HashMap<ReadingKey, CurrentReading>> {
    if keys.is_empty() {
        return Ok(HashMap::new());
    }
    let streams: Vec<Uuid> = keys.iter().map(|(s, _, _)| *s).collect();
    let times: Vec<DateTime<Utc>> = keys.iter().map(|(_, t, _)| *t).collect();
    let r = crate::common::served::r();
    let s = Alias::new("s");
    let query = Query::select()
        .column((r.clone(), readings::Column::StreamId))
        .column((r.clone(), readings::Column::Time))
        .column((r.clone(), readings::Column::ReplicateIndex))
        .column((r.clone(), readings::Column::SiteId))
        .column((r.clone(), readings::Column::MeasurementType))
        .expr_as(
            Func::coalesce([
                Expr::col((r.clone(), readings::Column::CalibratedValue)),
                Expr::col((r.clone(), readings::Column::RawValue)),
            ]),
            Alias::new("value"),
        )
        .expr_as(
            crate::routes::private::tools::service::reading_revision_expr(),
            Alias::new("revision"),
        )
        .column((s.clone(), data_streams::Column::SiteParameterId))
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::LeftJoin,
            data_streams::Entity,
            s.clone(),
            Expr::col((s.clone(), data_streams::Column::Id))
                .equals((r.clone(), readings::Column::StreamId)),
        )
        .and_where(Expr::col((r.clone(), readings::Column::StreamId)).is_in(streams))
        .and_where(Expr::col((r.clone(), readings::Column::Time)).is_in(times))
        .to_owned();
    let rows = db.query_all_raw(super::service::build(query)).await?;
    let mut out = HashMap::new();
    for row in rows.iter().map(|r| CurrentRow::from_query_result(r, "")) {
        let row = row?;
        let point = match (row.site_id, row.site_parameter_id) {
            (Some(site_id), Some(site_parameter_id)) => Some(SlotRef {
                site_id,
                site_parameter_id,
                time: row.time,
                // A derived row plots on the continuous line, the arm its record is read by.
                measurement_type: match row.measurement_type.as_deref() {
                    Some("spot") => "spot".to_string(),
                    _ => "continuous".to_string(),
                },
            }),
            _ => None,
        };
        out.insert(
            (row.stream_id, row.time, row.replicate_index),
            CurrentReading {
                revision: row.revision,
                value: row.value,
                point,
            },
        );
    }
    Ok(out)
}

/// What one entity row holds now, for the subjects a capture named.
#[derive(Debug, Clone)]
enum EntityValue {
    Number(f64),
    Site(serde_json::Value),
    Curve { slope: f64, intercept: f64 },
    Formula(String),
}

impl EntityValue {
    /// The value under the binding the input recorded: a site input reads one column of the row,
    /// everything else reads the whole thing.
    fn read(&self, input: &ConsumedInput) -> Option<serde_json::Value> {
        match self {
            Self::Number(value) => Some(serde_json::json!(value)),
            Self::Site(row) => {
                let column = input.property.as_deref()?;
                row.get(column).cloned().filter(|v| !v.is_null())
            }
            Self::Curve { slope, intercept } => {
                Some(serde_json::json!({ "slope": slope, "intercept": intercept }))
            }
            Self::Formula(text) => Some(serde_json::Value::String(text.clone())),
        }
    }
}

/// `<kind>:<uuid>` as the capture writes it. A subject in any other shape names nothing here.
fn subject_ids(subjects: &[String], kind: &str) -> Vec<Uuid> {
    subjects
        .iter()
        .filter_map(|s| s.strip_prefix(kind)?.parse().ok())
        .collect()
}

async fn entity_values<C: ConnectionTrait>(
    db: &C,
    subjects: &[String],
) -> AppResult<HashMap<String, EntityValue>> {
    let mut out = HashMap::new();
    let constant_ids = subject_ids(subjects, "constant:");
    if !constant_ids.is_empty() {
        for row in constants::Entity::find()
            .filter(constants::Column::Id.is_in(constant_ids))
            .all(db)
            .await?
        {
            out.insert(
                format!("constant:{}", row.id),
                EntityValue::Number(row.value),
            );
        }
    }
    let site_ids = subject_ids(subjects, "site:");
    if !site_ids.is_empty() {
        for row in sites::Entity::find()
            .filter(sites::Column::Id.is_in(site_ids))
            .all(db)
            .await?
        {
            let id = row.id;
            let value = serde_json::to_value(row).unwrap_or(serde_json::Value::Null);
            out.insert(format!("site:{id}"), EntityValue::Site(value));
        }
    }
    let curve_ids = subject_ids(subjects, "standard_curve:");
    if !curve_ids.is_empty() {
        for row in standard_curves::Entity::find()
            .filter(standard_curves::Column::Id.is_in(curve_ids))
            .all(db)
            .await?
        {
            out.insert(
                format!("standard_curve:{}", row.id),
                EntityValue::Curve {
                    slope: row.slope,
                    intercept: row.intercept,
                },
            );
        }
    }
    let formula_ids = subject_ids(subjects, "calculation_formula:");
    if !formula_ids.is_empty() {
        for row in calculation_formulas::Entity::find()
            .filter(calculation_formulas::Column::Id.is_in(formula_ids))
            .all(db)
            .await?
        {
            out.insert(
                format!("calculation_formula:{}", row.id),
                EntityValue::Formula(row.formula.clone()),
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
#[path = "tests/consumed.rs"]
mod tests;
