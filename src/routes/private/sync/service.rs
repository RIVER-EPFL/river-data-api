use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, EntityTrait, QueryFilter,
    QueryOrder, Set, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::routes::private::sensors::identity::{
    InstrumentKind, create_sensor_for_stream, upsert_source_instrument,
};
use crate::routes::private::{
    data_streams, data_streams::pairing_plans, parameters, projects, sensors,
    sensors::standard_curves, sites, sites::parameters as site_parameters,
};

/// How many plan entries an apply pairs between progress reports.
const PROGRESS_BATCH: usize = 25;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamHierarchy {
    pub project: String,
    pub site: String,
    pub parameter: String,
    /// Human-readable label for the parameter (the portal's dropdown text). The `parameter`
    /// field itself is the source's machine identity (its DB column name), which is what a
    /// scientist looking at the portal's own tables recognises.
    pub parameter_label: Option<String>,
    pub units: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub altitude_m: Option<f64>,
}

/// Extract the project/site/parameter hierarchy from a stream's metadata.
///
/// Priority:
/// 1. metadata.hierarchy (set by all portal backends)
/// 2. source_path segment parsing (fallback)
/// 3. source_name splitting on " - " (last resort)
pub fn extract_hierarchy(stream: &data_streams::Model) -> StreamHierarchy {
    let meta = &stream.metadata;

    // Try metadata.hierarchy first
    if let Some(h) = meta.get("hierarchy") {
        let project = h
            .get("project")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let site = h
            .get("site")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let parameter = h
            .get("parameter")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let parameter_label = h
            .get("parameter_label")
            .and_then(|v| v.as_str())
            .or_else(|| {
                meta.get("parameter")
                    .and_then(|p| p.get("display_name"))
                    .and_then(|v| v.as_str())
            })
            .filter(|l| !l.is_empty() && *l != parameter)
            .map(ToString::to_string);
        let units = meta
            .get("units")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let coords = meta.get("coordinates");
        let lat = coords
            .and_then(|c| c.get("latitude"))
            .and_then(|v| v.as_f64());
        let lon = coords
            .and_then(|c| c.get("longitude"))
            .and_then(|v| v.as_f64());
        let alt = coords
            .and_then(|c| c.get("altitude_m"))
            .and_then(|v| v.as_f64());

        if !project.is_empty() || !site.is_empty() || !parameter.is_empty() {
            return StreamHierarchy {
                project,
                site,
                parameter,
                parameter_label,
                units,
                latitude: lat,
                longitude: lon,
                altitude_m: alt,
            };
        }
    }

    // Fallback: source_path segment parsing
    if let Some(ref path) = stream.source_path {
        let segs: Vec<&str> = path.split('/').collect();
        let project = segs.get(1).unwrap_or(&"").to_string();
        let site = segs.get(2).unwrap_or(&"").to_string();
        let parameter = segs.get(3).unwrap_or(&"").to_string();
        let units = meta
            .get("units")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        return StreamHierarchy {
            project,
            site,
            parameter,
            parameter_label: None,
            units,
            latitude: None,
            longitude: None,
            altitude_m: None,
        };
    }

    // Last resort: source_name, stripping the "{site} - " prefix without truncating
    // display names that themselves contain " - "
    let parameter = stream
        .source_name
        .as_deref()
        .and_then(|n| n.splitn(2, " - ").nth(1))
        .unwrap_or("")
        .to_string();
    let units = meta
        .get("units")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    StreamHierarchy {
        project: stream.source_system.to_uppercase(),
        site: String::new(),
        parameter,
        parameter_label: None,
        units,
        latitude: None,
        longitude: None,
        altitude_m: None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEntry {
    pub stream_id: Uuid,
    pub source_key: String,
    pub source_name: Option<String>,
    pub action: String, // "pair" | "skip"
    pub project: PlanEntityRef,
    pub site: PlanSiteRef,
    pub parameter: PlanParamRef,
    pub confidence: String, // "exact" | "none"
    #[serde(default)]
    pub warnings: Vec<PlanWarning>,
    #[serde(default)]
    pub original_parameter_name: Option<String>,
    /// Present when the stream is a replicate family: what is being paired is the group of
    /// member columns, not the portal's average.
    #[serde(default)]
    pub replicates: Option<PlanReplicates>,
    /// The lab instrument this stream's standard curves belong to. Present when the stream names
    /// an instrument already, or when its replicate spec names a curve column, and absent
    /// otherwise. A curve is fitted on one instrument, so a reading naming a curve must name that
    /// instrument too; a stream that will carry curve references and resolves to no instrument has
    /// its readings refused (`/readings/batch`) or dropped (`/ingest`), which is what makes this a
    /// decision the plan has to settle rather than report.
    #[serde(default)]
    pub instrument: Option<PlanInstrumentRef>,
    /// The divisor this slot will publish its replicate standard deviation with, chosen in the
    /// review. Applied to the `site_parameters` row when the plan is applied; left unset, the slot
    /// stays undeclared and its audit disagreements are held for a decision instead.
    #[serde(default)]
    pub sd_estimator: Option<String>,
    /// The decimal places the source declared for this stream, written onto the slot on apply
    /// where the slot declares none. An operator's declaration on the slot is never overwritten.
    #[serde(default)]
    pub decimal_places: Option<i16>,
    /// The evidence for that choice: open replicate-statistics holds on this stream, and how many
    /// of them match the population signature. Written at plan creation so the review shows what
    /// the incoming data reports rather than only that a question exists.
    #[serde(default)]
    pub sd_holds: i64,
    #[serde(default)]
    pub sd_population_holds: i64,
    /// A person has looked at this entry and agreed with it. Set explicitly, never inferred from
    /// an edit: an operator who toggles a parameter group to skip and back has decided nothing.
    /// Only [`ReviewState::NeedsChecking`] entries wait on it; a fully matched entry with no
    /// warning is self-validated and needs no tick.
    #[serde(default)]
    pub acknowledged: bool,
    /// Whether the source reports this feed as a device. That, not the presence of a serial, is
    /// what makes a feed field-shaped: its instrument is minted from the feed's own provenance
    /// when the stream is paired, so the plan proposes no lab instrument for it. A source may
    /// describe a device and report no serial for it, which is why the two are separate.
    #[serde(default)]
    pub is_device: bool,
    /// The device serial the source names for this feed, where it names one. Information the plan
    /// displays; never the instrument's identity.
    #[serde(default)]
    pub device_serial: Option<String>,
    /// The device model, where the source reports one. Naming only.
    #[serde(default)]
    pub device_model: Option<String>,
}

/// A catalog parameter a plan entry collides with, and what already depends on it. "Exists" on its
/// own does not say where or whether anything uses it, which is the question an operator has to
/// answer to resolve a units conflict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExistingParamRef {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub units: String,
    pub category: String,
    pub site_parameter_count: i64,
    pub reading_count: i64,
}

/// Something the review has to decide about, carried as data rather than a sentence so the UI can
/// offer the resolutions instead of only naming the problem. `message` is the rendered form, kept
/// so a warning always reads as something even where the structure is not used.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanWarning {
    /// `units_mismatch` | `empty_name`.
    pub kind: String,
    pub message: String,
    #[serde(default)]
    pub parameter: Option<String>,
    #[serde(default)]
    pub existing: Option<ExistingParamRef>,
    /// The units this source declares, against `existing.units`.
    #[serde(default)]
    pub source_units: Option<String>,
}

impl PlanWarning {
    pub fn units_mismatch(parameter: &str, existing: &CatalogParam, source_units: &str) -> Self {
        Self {
            kind: "units_mismatch".to_string(),
            message: format!(
                "Parameter '{parameter}' exists in the catalog with units '{}' but this source \
                 uses '{source_units}'",
                existing.units
            ),
            parameter: Some(parameter.to_string()),
            existing: Some(ExistingParamRef {
                id: existing.id,
                code: existing.code.clone(),
                name: existing.name.clone(),
                units: existing.units.clone(),
                category: existing.category.clone(),
                site_parameter_count: existing.site_parameter_count,
                reading_count: existing.reading_count,
            }),
            source_units: Some(source_units.to_string()),
        }
    }

    pub fn empty_name() -> Self {
        Self {
            kind: "empty_name".to_string(),
            message: "site or parameter name is empty".to_string(),
            parameter: None,
            existing: None,
            source_units: None,
        }
    }

    /// This source ships its own precomputed standard deviation and nothing has declared which
    /// divisor it uses. The pairing is where that can first be asked, so it is asked here, with
    /// the open holds matching the population signature as the evidence; leaving it unset is
    /// allowed and the audit gate is the backstop.
    pub fn sd_estimator_undeclared(parameter: &str, population_holds: i64) -> Self {
        let message = if population_holds == 0 {
            format!(
                "'{parameter}' ships its own standard deviation and no divisor is declared for \
                 it. Declare which one this source uses."
            )
        } else {
            format!(
                "{population_holds} incoming standard deviation{} for '{parameter}' match the \
                 population divisor (n), not ours. Declare which one this source uses.",
                if population_holds == 1 { "" } else { "s" }
            )
        };
        Self {
            kind: "sd_estimator_undeclared".to_string(),
            message,
            parameter: Some(parameter.to_string()),
            existing: None,
            source_units: None,
        }
    }
}

/// One of an instrument's standard curves, carried so the review can show what a save would
/// correct with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanCurveRef {
    pub id: Uuid,
    pub name: Option<String>,
    pub slope: f64,
    pub intercept: f64,
}

/// The instrument a plan entry's curve references resolve to, and how that was decided.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanInstrumentRef {
    /// The source column naming a curve per reading, e.g. `doc_std_curve_id`. Absent when the
    /// instrument came from the stream and no column names a curve (the chla families, corrected
    /// upstream).
    #[serde(default)]
    pub curve_column: Option<String>,
    /// The resolved instrument, or None when one has to be created.
    pub id: Option<Uuid>,
    pub name: String,
    /// `(source_system, source_key)` is an instrument's identity, so a later rename cannot break
    /// the mapping.
    pub source_key: String,
    /// `stream` (already attributed), `curve_label` (matched against the source's own curve
    /// labels), `manual` (repointed in the review), or `placeholder` (nothing matched).
    pub resolved_by: String,
    pub create: bool,
    /// A creation an operator has agreed to. Apply refuses a plan holding an unconfirmed one.
    #[serde(default)]
    pub confirmed: bool,
    /// True when each reading stores a `standard_curve_id` (the family's own calculation names
    /// the curve, members are raw). False when the curve was applied upstream and only the
    /// instrument is attributed, where stamping would correct the value a second time.
    pub stamps_readings: bool,
    #[serde(default)]
    pub curves: Vec<PlanCurveRef>,
    /// The name this decision proposes creating, kept whatever else the entry resolves to. An
    /// operator who attaches an existing instrument by mistake has the proposal to go back to;
    /// without it, the only record of what the plan suggested is gone the moment it is overwritten.
    #[serde(default)]
    pub proposed_name: Option<String>,
    /// An instrument that already carries the proposed name. Creating a second one under it is
    /// allowed, and so is attaching to this one, but neither may happen by default: readings
    /// joining an instrument that already holds data is not something a plan decides on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_conflict: Option<InstrumentNameConflict>,
}

/// The instrument a proposed name collides with, enough of it to choose by.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstrumentNameConflict {
    pub id: Uuid,
    pub name: String,
    /// Where it came from, so an operator can tell a hand entry from an earlier import.
    pub source_system: Option<String>,
    /// True when it already carries readings; attaching adds to them.
    pub has_readings: bool,
}

/// Replicate-family summary carried on a plan entry, from the stream's registered spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanReplicates {
    pub n: usize,
    pub member_columns: Vec<String>,
    pub curve_ref_column: Option<String>,
    pub portal_mean_column: Option<String>,
    pub portal_sd_column: Option<String>,
}

/// The instruments a source has registered, with their curves, plus any instrument the plan's
/// streams already name (which may belong to no source, e.g. a device registered by serial).
pub struct InstrumentCatalog {
    /// Instrument id -> (display name, source_key).
    by_id: HashMap<Uuid, (String, Option<String>)>,
    /// The source's own instruments, as (normalised label, id), for curve-column matching.
    labels: Vec<(String, Uuid)>,
    /// The source's own instruments by `source_key`, which is the identity an apply mints and
    /// dedupes on. Looked up before anything is proposed, so a plan built after an earlier one
    /// reports the instrument it already created rather than asking to create it again.
    by_source_key: HashMap<String, Uuid>,
    /// **Every** instrument by lowercased name, this source's and everyone else's. A name
    /// collision is about what a person reads, so it does not stop at the source boundary: the
    /// lab's `DOC` may have arrived by hand or from another import.
    by_name: HashMap<String, InstrumentNameConflict>,
    curves: HashMap<Uuid, Vec<PlanCurveRef>>,
}

impl InstrumentCatalog {
    /// The instrument already carrying this name, if any.
    #[must_use]
    pub fn named(&self, name: &str) -> Option<InstrumentNameConflict> {
        self.by_name.get(&name.trim().to_lowercase()).cloned()
    }
}

/// A curve column's stem, normalised for comparison against an instrument label:
/// `doc_std_curve_id` -> `doc`, `chla_acid_std_curve_id` -> `chla acid`.
fn curve_column_stem(column: &str) -> String {
    column
        .to_lowercase()
        .trim_end_matches("_std_curve_id")
        .replace('_', " ")
        .trim()
        .to_string()
}

/// An instrument's label, normalised the same way. The source prefix is dropped because it is
/// already the thing being matched within.
fn instrument_label(source_key: &str, source_system: &str) -> String {
    source_key
        .strip_prefix(&format!("{source_system}:"))
        .unwrap_or(source_key)
        .to_lowercase()
        .replace('_', " ")
        .trim()
        .to_string()
}

pub async fn load_instrument_catalog(
    db: &impl ConnectionTrait,
    source_system: &str,
    named_ids: &[Uuid],
) -> AppResult<InstrumentCatalog> {
    let rows = sensors::Entity::find()
        .filter(
            Condition::any()
                .add(sensors::Column::SourceSystem.eq(source_system))
                .add(sensors::Column::Id.is_in(named_ids.to_vec())),
        )
        .all(db)
        .await?;

    // Names are read across every source: see `by_name`.
    let named_rows = sensors::Entity::find().all(db).await?;
    let with_readings: std::collections::HashSet<Uuid> = db
        .query_all_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT DISTINCT sensor_id FROM readings WHERE sensor_id IS NOT NULL".to_string(),
        ))
        .await?
        .iter()
        .filter_map(|r| r.try_get::<Uuid>("", "sensor_id").ok())
        .collect();
    let mut by_name: HashMap<String, InstrumentNameConflict> = HashMap::new();
    for row in &named_rows {
        let Some(name) = row
            .name
            .as_ref()
            .map(|n| n.trim())
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        by_name
            .entry(name.to_lowercase())
            .or_insert_with(|| InstrumentNameConflict {
                id: row.id,
                name: name.to_string(),
                source_system: row.source_system.clone(),
                has_readings: with_readings.contains(&row.id),
            });
    }

    let mut by_id = HashMap::new();
    let mut labels = Vec::new();
    let mut by_source_key = HashMap::new();
    for row in &rows {
        let name = row
            .name
            .clone()
            .or_else(|| row.serial_number.clone())
            .unwrap_or_else(|| row.id.to_string());
        if row.source_system.as_deref() == Some(source_system)
            && let Some(key) = &row.source_key
        {
            labels.push((instrument_label(key, source_system), row.id));
            by_source_key.insert(key.clone(), row.id);
        }
        by_id.insert(row.id, (name, row.source_key.clone()));
    }

    let ids: Vec<Uuid> = by_id.keys().copied().collect();
    let mut curves: HashMap<Uuid, Vec<PlanCurveRef>> = HashMap::new();
    if !ids.is_empty() {
        for c in standard_curves::Entity::find()
            .filter(standard_curves::Column::SensorId.is_in(ids))
            .all(db)
            .await?
        {
            curves.entry(c.sensor_id).or_default().push(PlanCurveRef {
                id: c.id,
                name: c.name.clone(),
                slope: c.slope,
                intercept: c.intercept,
            });
        }
    }

    Ok(InstrumentCatalog {
        by_id,
        labels,
        by_source_key,
        by_name,
        curves,
    })
}

/// Which instrument a stream's curve references belong to, most specific first: the instrument the
/// stream already names, then the source's own curve labels matched against the curve column, then
/// a placeholder for an operator to confirm.
///
/// The label match is what lets a portal whose curve column is empty in the data still resolve: the
/// curve catalog is replicated independently of the readings, so the instrument is knowable even
/// when no row has yet named a curve. It is a heuristic, so it is reported as one, and an
/// ambiguous stem resolves to nothing rather than to a guess.
pub fn resolve_instrument(
    stream_sensor_id: Option<Uuid>,
    curve_column: Option<&str>,
    source_system: &str,
    catalog: &InstrumentCatalog,
) -> Option<PlanInstrumentRef> {
    let stamps_readings = curve_column.is_some();
    let curve_column = curve_column.map(str::to_string);

    if let Some(id) = stream_sensor_id {
        let (name, source_key) = catalog
            .by_id
            .get(&id)
            .cloned()
            .unwrap_or_else(|| (id.to_string(), None));
        return Some(PlanInstrumentRef {
            curve_column,
            id: Some(id),
            name,
            source_key: source_key.unwrap_or_default(),
            resolved_by: "stream".to_string(),
            create: false,
            confirmed: true,
            stamps_readings,
            curves: catalog.curves.get(&id).cloned().unwrap_or_default(),
            proposed_name: None,
            name_conflict: None,
        });
    }

    let column = curve_column.clone()?;
    let stem = curve_column_stem(&column);
    let source_key = format!("{source_system}:{column}");

    // An instrument this source already has under the key an apply would mint is that decision,
    // already taken. Looking it up before proposing is what keeps a second plan from re-asking.
    if let Some(id) = catalog.by_source_key.get(&source_key).copied() {
        let (name, key) = catalog.by_id.get(&id).cloned().unwrap_or_default();
        return Some(PlanInstrumentRef {
            curve_column,
            id: Some(id),
            name,
            source_key: key.unwrap_or(source_key),
            resolved_by: "source_key".to_string(),
            create: false,
            confirmed: true,
            stamps_readings,
            curves: catalog.curves.get(&id).cloned().unwrap_or_default(),
            proposed_name: None,
            name_conflict: None,
        });
    }

    let matches: Vec<Uuid> = catalog
        .labels
        .iter()
        .filter(|(label, _)| *label == stem || label.starts_with(&format!("{stem} ")))
        .map(|(_, id)| *id)
        .collect();

    if let [id] = matches[..] {
        let (name, source_key) = catalog.by_id.get(&id).cloned().unwrap_or_default();
        return Some(PlanInstrumentRef {
            curve_column,
            id: Some(id),
            name,
            source_key: source_key.unwrap_or_default(),
            resolved_by: "curve_label".to_string(),
            create: false,
            confirmed: true,
            stamps_readings,
            curves: catalog.curves.get(&id).cloned().unwrap_or_default(),
            proposed_name: None,
            name_conflict: None,
        });
    }

    let name = format!("{stem} {source_system}");
    Some(PlanInstrumentRef {
        curve_column: Some(column),
        id: None,
        name: name.clone(),
        source_key,
        resolved_by: "placeholder".to_string(),
        create: true,
        confirmed: true,
        stamps_readings,
        curves: vec![],
        proposed_name: Some(name),
        name_conflict: None,
    })
}

/// The provenance key a stream's instrument is held under: the source's instrument for the raw
/// column the feed carries, or the feed's own key when it names no parameter.
///
/// The plan and the pairing both key through here, so the row one proposes is the row the other
/// mints. A plan's suggested parameter is a display name (a family's `DOC_avg_ppb` reads as `DOC`)
/// and the regrouping loop rewrites it again, so keying off it mints a second instrument for the
/// same analyte.
pub fn stream_instrument_key(stream: &data_streams::Model) -> String {
    let parameter = extract_hierarchy(stream).parameter;
    let key_part = if parameter.is_empty() {
        stream.source_key.as_str()
    } else {
        parameter.as_str()
    };
    format!("{}:{key_part}", stream.source_system)
}

/// The instrument a source parameter resolves to, for the feeds that name no curve column.
///
/// The source's own instrument under the key an apply mints ([`stream_instrument_key`]) when it has
/// one, and otherwise that same key proposed for creation, pre-agreed. Every stream is paired with
/// an instrument, so the review's default is the suggestion rather than a question: an operator who
/// wants another instrument attaches it, and one who wants none has nothing to pair. `parameter`
/// names the proposal, it does not key it.
pub fn resolve_parameter_instrument(
    source_key: String,
    parameter: &str,
    catalog: &InstrumentCatalog,
) -> PlanInstrumentRef {
    if let Some(id) = catalog.by_source_key.get(&source_key).copied() {
        let (name, key) = catalog.by_id.get(&id).cloned().unwrap_or_default();
        return PlanInstrumentRef {
            curve_column: None,
            id: Some(id),
            name,
            source_key: key.unwrap_or(source_key),
            resolved_by: "source_key".to_string(),
            create: false,
            confirmed: true,
            stamps_readings: false,
            curves: catalog.curves.get(&id).cloned().unwrap_or_default(),
            proposed_name: None,
            name_conflict: None,
        };
    }
    // The lab's DOC analyser is one machine carried to every station, so it is called DOC. The
    // source is provenance, held in `source_key`, and putting it in the name would make every
    // import read as a different instrument to the person choosing between them.
    let name = parameter.to_string();
    let conflict = catalog.named(&name);
    PlanInstrumentRef {
        curve_column: None,
        id: None,
        name: name.clone(),
        source_key,
        resolved_by: "parameter".to_string(),
        create: true,
        // A name an instrument already carries is a decision, not a proposal: the readings would
        // join a row that already holds data, so the operator says which they meant.
        confirmed: conflict.is_none(),
        stamps_readings: false,
        curves: vec![],
        proposed_name: Some(name),
        name_conflict: conflict,
    }
}

fn plan_replicates(metadata: &serde_json::Value) -> Option<PlanReplicates> {
    let spec =
        crate::routes::private::data_streams::replicates::ReplicateSpec::from_metadata(metadata)?;
    Some(PlanReplicates {
        n: spec.source_columns.len(),
        member_columns: spec.source_columns,
        curve_ref_column: spec.curve_ref_column,
        portal_mean_column: spec.portal_mean_column,
        portal_sd_column: spec.portal_sd_column,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEntityRef {
    pub id: Option<Uuid>,
    pub name: String,
    pub create: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSiteRef {
    pub id: Option<Uuid>,
    pub name: String,
    pub create: bool,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub altitude_m: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanParamRef {
    pub id: Option<Uuid>,
    /// The parameter identity: the source's own column name (matches what the portal DB shows).
    pub name: String,
    /// Human-readable label carried alongside; becomes `parameters.name` when the apply creates
    /// the parameter, while `name` becomes its `code`.
    #[serde(default)]
    pub label: Option<String>,
    pub create: bool,
    pub units: String,
    #[serde(default)]
    pub group_key: Option<String>,
    #[serde(default)]
    pub original_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSummary {
    pub total_streams: usize,
    pub will_pair: usize,
    /// The three review states over the entries the plan would pair, so the review can say what
    /// share of the plan waits on a person and what share stands on its own evidence.
    #[serde(default)]
    pub needs_checking: usize,
    #[serde(default)]
    pub self_validated: usize,
    #[serde(default)]
    pub acknowledged: usize,
    pub will_skip: usize,
    pub projects_to_create: usize,
    pub sites_to_create: usize,
    pub parameters_to_create: usize,
    /// Distinct lab instruments the apply would create, and how many of those an operator has
    /// not yet agreed to. Apply refuses while the second is non-zero.
    #[serde(default)]
    pub instruments_to_create: usize,
    #[serde(default)]
    pub instruments_unconfirmed: usize,
    pub unique_projects: usize,
    pub unique_sites: usize,
    pub unique_parameters: usize,
}

struct ParamGroupProposal {
    proposed_name: String,
    units: String,
    original_names: Vec<String>,
    entry_indices: Vec<usize>,
}

fn group_streams_by_parameter(entries: &[(usize, String, String)]) -> Vec<ParamGroupProposal> {
    // Distinct quantities can share a units suffix (e.g. "Nitrate [µg/L]" vs
    // "Ammonia [µg/L]"), so only entries whose names are identical group together.
    let mut by_key: HashMap<(String, String), Vec<(usize, String)>> = HashMap::new();
    for (idx, name, units) in entries {
        by_key
            .entry((units.to_lowercase(), name.to_lowercase()))
            .or_default()
            .push((*idx, name.clone()));
    }

    by_key
        .into_iter()
        .map(|((units, _), members)| {
            let mut original_names: Vec<String> = members.iter().map(|(_, n)| n.clone()).collect();
            original_names.sort();
            original_names.dedup();
            ParamGroupProposal {
                proposed_name: members[0].1.clone(),
                units,
                original_names,
                entry_indices: members.iter().map(|(idx, _)| *idx).collect(),
            }
        })
        .collect()
}

/// Create a pairing plan for all unpaired streams of a given source system.
pub async fn create_plan(
    db: &impl ConnectionTrait,
    source_system: &str,
) -> AppResult<pairing_plans::Model> {
    let streams = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq(source_system))
        .filter(data_streams::Column::SiteParameterId.is_null())
        .order_by_asc(data_streams::Column::SourceKey)
        .all(db)
        .await?;

    // A stream superseded by a replicate family (another stream at `source_key || ':reps'`) is a
    // retired legacy single whose stale metadata still carries the old label identity; planning it
    // would seed duplicate parameter rows. One query for the whole superseded set.
    let superseded: std::collections::HashSet<String> = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT ds.source_key FROM data_streams ds
             WHERE ds.source_system = $1
               AND EXISTS (SELECT 1 FROM data_streams fam
                           WHERE fam.source_system = ds.source_system
                             AND fam.source_key = ds.source_key || ':reps')",
            [source_system.to_string().into()],
        ))
        .await?
        .iter()
        .filter_map(|r| r.try_get::<String>("", "source_key").ok())
        .collect();
    let streams: Vec<data_streams::Model> = streams
        .into_iter()
        .filter(|s| !superseded.contains(&s.source_key))
        .collect();

    if streams.is_empty() {
        return Err(AppError::BadRequest(format!(
            "No unpaired streams found for source_system '{source_system}'"
        )));
    }

    let catalog = load_entity_catalog(db).await?;
    let named_instruments: Vec<Uuid> = streams.iter().filter_map(|s| s.sensor_id).collect();
    let instruments = load_instrument_catalog(db, source_system, &named_instruments).await?;

    // Divisor evidence per stream: its open replicate-statistics holds and how many carry the
    // population signature. The same signature SQL the audit list and gate use, so the numbers
    // the review quotes cannot disagree with the queue.
    let stream_ids: Vec<Uuid> = streams.iter().map(|s| s.id).collect();
    let sd_evidence: std::collections::HashMap<Uuid, (i64, i64)> = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT h.stream_id, count(*) AS holds, \
                        count(*) FILTER (WHERE {}) AS population \
                 FROM replicate_audit_holds h \
                 WHERE h.kind = 'replicate_stats' \
                   AND h.status IN ('pending', 'deferred') \
                   AND h.stream_id = ANY($1) \
                 GROUP BY h.stream_id",
                *super::replicate_audit::POPULATION_SD_SQL
            ),
            [stream_ids.into()],
        ))
        .await?
        .iter()
        .filter_map(|r| {
            Some((
                r.try_get::<Uuid>("", "stream_id").ok()?,
                (
                    r.try_get::<i64>("", "holds").ok()?,
                    r.try_get::<i64>("", "population").ok()?,
                ),
            ))
        })
        .collect();

    // Slots that already declare a divisor. A declaration is owned by the slot, so an entry landing
    // on one adopts what it says rather than asking again.
    let declared_slots: std::collections::HashMap<(Uuid, Uuid), String> = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT site_id, parameter_id, sd_estimator FROM site_parameters \
             WHERE sd_estimator IS NOT NULL",
        ))
        .await?
        .iter()
        .filter_map(|r| {
            Some((
                (
                    r.try_get::<Uuid>("", "site_id").ok()?,
                    r.try_get::<Uuid>("", "parameter_id").ok()?,
                ),
                r.try_get::<String>("", "sd_estimator").ok()?,
            ))
        })
        .collect();

    // Build entries
    let mut entries: Vec<PlanEntry> = Vec::with_capacity(streams.len());

    for stream in &streams {
        let h = extract_hierarchy(stream);

        let action = if h.site.is_empty() || h.parameter.is_empty() {
            "skip".to_string()
        } else {
            "pair".to_string()
        };

        // What is paired for a family is the replicate group, whose avg and sd this system
        // computes, so the suggested parameter is the measurand rather than the incoming
        // statistic column. The incoming name survives as original_parameter_name.
        let replicates = plan_replicates(&stream.metadata);
        let parameter_name = if replicates.is_some() && !h.parameter.is_empty() {
            family_parameter_suggestion(&h.parameter)
        } else {
            h.parameter.clone()
        };

        // The divisor is declared, never inferred: a family the review has not answered stays
        // undeclared, whatever its holds say, and the audit gate holds its disagreements until
        // someone does. The holds are carried as evidence for that answer, not as one.
        let (sd_holds, sd_population_holds) =
            sd_evidence.get(&stream.id).copied().unwrap_or((0, 0));
        let reports_sd = replicates
            .as_ref()
            .is_some_and(|r| r.portal_sd_column.is_some());

        let mut entry = PlanEntry {
            stream_id: stream.id,
            source_key: stream.source_key.clone(),
            source_name: stream.source_name.clone(),
            action,
            project: PlanEntityRef {
                id: None,
                name: h.project,
                create: false,
            },
            site: PlanSiteRef {
                id: None,
                name: h.site,
                create: false,
                latitude: h.latitude,
                longitude: h.longitude,
                altitude_m: h.altitude_m,
            },
            parameter: PlanParamRef {
                id: None,
                name: parameter_name,
                label: h.parameter_label.clone(),
                create: false,
                units: h.units,
                group_key: None,
                original_names: vec![],
            },
            confidence: "none".to_string(),
            warnings: vec![],
            original_parameter_name: Some(h.parameter),
            instrument: resolve_instrument(
                stream.sensor_id,
                replicates
                    .as_ref()
                    .and_then(|r| r.curve_ref_column.clone())
                    .as_deref(),
                source_system,
                &instruments,
            ),
            replicates,
            sd_estimator: None,
            decimal_places: crate::routes::private::data_streams::service::declared_decimal_places(
                &stream.metadata,
            ),
            sd_holds,
            sd_population_holds,
            acknowledged: false,
            is_device: crate::routes::private::sensors::identity::is_device_feed(&stream.metadata),
            device_serial: crate::routes::private::sensors::identity::extract_vaisala_device_serial(
                &stream.metadata,
            ),
            device_model: stream
                .metadata
                .get("device")
                .and_then(|d| d.get("logger_device"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        };
        reclassify_entry(&mut entry, &catalog);
        // A feed naming no curve column can still belong to an instrument this source created in
        // an earlier plan. A device-shaped feed is never one of those: its instrument is minted
        // from its own provenance at pairing.
        if entry.instrument.is_none() && !entry.is_device {
            entry.instrument = Some(resolve_parameter_instrument(
                stream_instrument_key(stream),
                &entry.parameter.name,
                &instruments,
            ));
        }
        if reports_sd
            && let (Some(site_id), Some(param_id)) = (entry.site.id, entry.parameter.id)
            && let Some(declared) = declared_slots.get(&(site_id, param_id))
        {
            entry.sd_estimator = Some(declared.clone());
            entry
                .warnings
                .retain(|w| w.kind != "sd_estimator_undeclared");
        }
        entries.push(entry);
    }

    // Group new-to-create parameters with identical names (per units) across sites
    let to_group: Vec<(usize, String, String)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.action == "pair" && e.parameter.create)
        .map(|(i, e)| (i, e.parameter.name.clone(), e.parameter.units.clone()))
        .collect();

    if !to_group.is_empty() {
        for group in group_streams_by_parameter(&to_group) {
            if group.entry_indices.len() <= 1 {
                continue;
            }
            let key = format!("{}::{}", group.units, group.proposed_name);
            for &idx in &group.entry_indices {
                entries[idx].parameter.name = group.proposed_name.clone();
                entries[idx].parameter.group_key = Some(key.clone());
                entries[idx].parameter.original_names = group.original_names.clone();
            }
        }
    }

    let summary = compute_summary(&entries);

    let plan = pairing_plans::ActiveModel {
        id: Set(Uuid::new_v4()),
        source_system: Set(source_system.to_string()),
        status: Set("draft".to_string()),
        created_by: Set(None),
        summary: Set(serde_json::to_value(&summary).unwrap_or_default()),
        entries: Set(serde_json::to_value(&entries).unwrap_or_default()),
        curve_assignments: Set(serde_json::Value::Array(Vec::new())),
        version: Set(0),
        created_at: Set(Utc::now().into()),
        applied_at: Set(None),
        apply_result: Set(None),
    };

    let inserted = plan.insert(db).await?;
    Ok(inserted)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApplyResult {
    pub projects_created: u32,
    pub sites_created: u32,
    pub parameters_created: u32,
    pub site_parameters_created: u32,
    pub streams_paired: u32,
    #[serde(default)]
    pub streams_skipped: u32,
    #[serde(default)]
    pub instruments_created: u32,
    /// Standard curves moved onto instruments this apply minted.
    #[serde(default)]
    pub curves_assigned: u32,
    pub readings_backfilled: u64,
}

/// A standard curve the review assigned to an instrument the plan creates, keyed by the
/// instrument's `source_key` because the row does not exist until the apply mints it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanCurveIntent {
    pub curve_id: Uuid,
    pub instrument_source_key: String,
}

/// The curve assignments a plan carries. Unreadable JSON is an internal error, not an empty list:
/// silently dropping an assignment is the failure this column exists to prevent.
pub fn plan_curve_intents(plan: &pairing_plans::Model) -> AppResult<Vec<PlanCurveIntent>> {
    serde_json::from_value(plan.curve_assignments.clone())
        .map_err(|e| AppError::Internal(format!("Failed to parse plan curve assignments: {e}")))
}

/// Move each assigned curve onto the instrument the apply minted for its `source_key`. Runs
/// inside the apply transaction, after `mint_plan_instruments`. An assignment naming an
/// instrument the plan no longer creates, or a curve readings already name, fails the apply
/// rather than being dropped: the review chose it, so nothing here may quietly not do it.
async fn assign_plan_curves<C: ConnectionTrait>(
    txn: &C,
    intents: &[PlanCurveIntent],
    minted: &HashMap<String, Uuid>,
) -> AppResult<u32> {
    let mut moved = 0u32;
    for intent in intents {
        let Some(&sensor_id) = minted.get(&intent.instrument_source_key) else {
            return Err(AppError::BadRequest(format!(
                "curve {} is assigned to instrument '{}', which this plan no longer creates; \
                 reassign or clear the curve before applying",
                intent.curve_id, intent.instrument_source_key
            )));
        };
        if crate::routes::private::sensors::standard_curves::views::curve_is_used(
            txn,
            intent.curve_id,
        )
        .await?
        {
            return Err(AppError::BadRequest(format!(
                "curve {} has already been applied to readings, so its instrument is fixed; \
                 clear the assignment before applying",
                intent.curve_id
            )));
        }
        let result = txn
            .execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE standard_curves SET sensor_id = $1 WHERE id = $2",
                [sensor_id.into(), intent.curve_id.into()],
            ))
            .await?;
        if result.rows_affected() == 0 {
            return Err(AppError::BadRequest(format!(
                "curve {} no longer exists; clear the assignment before applying",
                intent.curve_id
            )));
        }
        moved += 1;
    }
    Ok(moved)
}

struct EntityCaches {
    projects: HashMap<String, Uuid>,
    sites: HashMap<String, Uuid>,
    params: HashMap<String, Uuid>,
    site_params: HashMap<(Uuid, Uuid), Uuid>,
    param_names: HashMap<Uuid, String>,
}

struct ApplyCounters {
    projects_created: u32,
    sites_created: u32,
    params_created: u32,
    sp_created: u32,
    streams_paired: u32,
    streams_skipped: u32,
    instruments_created: u32,
    curves_assigned: u32,
}

/// The streams whose curve references resolve to an instrument nobody has agreed to create.
pub fn unconfirmed_instruments(entries: &[PlanEntry]) -> Vec<&str> {
    entries
        .iter()
        .filter(|e| e.action == "pair")
        .filter(|e| {
            e.instrument
                .as_ref()
                .is_some_and(|i| i.create && !i.confirmed)
        })
        .map(|e| e.source_key.as_str())
        .collect()
}

/// An instrument nobody agreed to is not created silently. Refusing rather than pairing anyway is
/// the point: a stream that will carry curve references and names no instrument has those readings
/// refused by `/readings/batch` and dropped by `/ingest`, so pairing it in that state builds the
/// failure in.
pub fn refuse_unconfirmed_instruments(entries: &[PlanEntry]) -> AppResult<()> {
    let unconfirmed = unconfirmed_instruments(entries);
    if unconfirmed.is_empty() {
        return Ok(());
    }
    Err(AppError::BadRequest(format!(
        "{} stream(s) need an instrument for their standard curves before they can pair: {}",
        unconfirmed.len(),
        unconfirmed
            .iter()
            .take(5)
            .copied()
            .collect::<Vec<_>>()
            .join(", "),
    )))
}

/// Apply a pairing plan: create entities, pair streams, backfill readings.
pub async fn apply_plan(
    db: &sea_orm::DatabaseConnection,
    plan_id: Uuid,
    progress: Option<&crate::routes::private::reprocessing_jobs::lifecycle::JobContext>,
) -> AppResult<ApplyResult> {
    let plan = pairing_plans::Entity::find_by_id(plan_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    if plan.status != "draft" {
        return Err(AppError::BadRequest(format!(
            "Plan is '{}', can only apply 'draft' plans",
            plan.status
        )));
    }

    let entries: Vec<PlanEntry> = serde_json::from_value(plan.entries.clone())
        .map_err(|e| AppError::Internal(format!("Failed to parse plan entries: {e}")))?;
    let curve_intents = plan_curve_intents(&plan)?;

    refuse_unconfirmed_instruments(&entries)?;
    if let Some(reason) =
        crate::routes::private::data_streams::service::pairing_refusal(&plan.source_system)
    {
        return Err(AppError::BadRequest(reason));
    }

    let txn = db.begin().await?;

    // Atomic status claim: a concurrent apply of the same plan matches zero rows and bails.
    // A rollback restores 'draft'.
    let claimed = txn
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE pairing_plans SET status = 'applying' WHERE id = $1 AND status = 'draft'",
            [plan_id.into()],
        ))
        .await?;
    if claimed.rows_affected() == 0 {
        return Err(AppError::BadRequest(
            "Plan is no longer in draft status".to_string(),
        ));
    }

    crate::common::bulk_write::lift_decompression_cap(&txn).await?;

    let param_names: HashMap<Uuid, String> = parameters::Entity::find()
        .all(&txn)
        .await?
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();

    let mut caches = EntityCaches {
        projects: HashMap::new(),
        sites: HashMap::new(),
        params: HashMap::new(),
        site_params: HashMap::new(),
        param_names,
    };
    let mut counters = ApplyCounters {
        projects_created: 0,
        sites_created: 0,
        params_created: 0,
        sp_created: 0,
        streams_paired: 0,
        streams_skipped: 0,
        instruments_created: 0,
        curves_assigned: 0,
    };

    let minted = mint_plan_instruments(&txn, &plan.source_system, &entries).await?;
    counters.instruments_created = minted.len() as u32;
    counters.curves_assigned = assign_plan_curves(&txn, &curve_intents, &minted).await?;

    // How far the apply has got, on the pool connection rather than inside `txn`, so the operator
    // sees an import of a couple of thousand entries move instead of a spinner.
    let pairing_total = entries.iter().filter(|e| e.action == "pair").count();
    if let Some(ctx) = progress {
        ctx.set_progress(0, Some(i32::try_from(pairing_total).unwrap_or(i32::MAX)))
            .await;
    }
    let mut entries_seen: usize = 0;

    for entry in entries.iter().filter(|e| e.action == "pair") {
        entries_seen += 1;
        if let Some(ctx) = progress
            && (entries_seen % PROGRESS_BATCH == 0 || entries_seen == pairing_total)
        {
            ctx.set_progress(i32::try_from(entries_seen).unwrap_or(i32::MAX), None)
                .await;
        }
        if (entry.site.id.is_none() && entry.site.name.trim().is_empty())
            || (entry.parameter.id.is_none() && entry.parameter.name.trim().is_empty())
        {
            tracing::warn!(
                stream_id = %entry.stream_id,
                "apply_plan: skipping entry with empty site or parameter name",
            );
            counters.streams_skipped += 1;
            continue;
        }
        let Some(stream) = data_streams::Entity::find_by_id(entry.stream_id)
            .one(&txn)
            .await?
        else {
            tracing::warn!(
                stream_id = %entry.stream_id,
                "apply_plan: skipping entry whose stream no longer exists",
            );
            counters.streams_skipped += 1;
            continue;
        };
        // Checked before resolving so a skipped entry leaves no orphan site or parameter behind.
        if let Some(existing_sp) = stream.site_parameter_id {
            tracing::warn!(
                stream_id = %entry.stream_id,
                site_parameter_id = %existing_sp,
                "apply_plan: skipping stream that is already paired",
            );
            counters.streams_skipped += 1;
            continue;
        }
        let (site_parameter_id, parameter_id) =
            resolve_plan_entry(&txn, entry, &plan.source_system, &mut caches, &mut counters)
                .await?;
        let instrument_id = entry
            .instrument
            .as_ref()
            .and_then(|i| i.id.or_else(|| minted.get(&i.source_key).copied()));
        pair_entry_stream(
            &txn,
            stream,
            plan_id,
            site_parameter_id,
            parameter_id,
            instrument_id,
        )
        .await?;
        counters.streams_paired += 1;
    }

    let backfilled = backfill_plan_readings(&txn, plan_id).await?;
    let readings_backfilled = backfilled.readings;
    finalize_plan(&txn, plan_id, &counters, readings_backfilled).await?;
    txn.commit().await?;

    // Attribution is what made these readings visit values; the calculations that read them at
    // each manual visit run now (ADR 0007). The plan runs as a job, so the writer it records is
    // the system rather than a person.
    crate::routes::private::collection_events::recompute::enqueue_for(
        db,
        &backfilled.touched_events,
        "system",
        crate::routes::private::collection_events::recompute::Writer::Person,
    )
    .await?;

    // Re-derive the paired readings by the deployment + calibration windows for each touched
    // (site, parameter) slot, then a full refresh as a safety net. `backfill_plan_readings` only
    // stamps site_id/parameter_id; the window-aware engine (same one ingest/reprocess use) assigns
    // sensor_id/deployment_id/calibration_id and the per-window calibrated_value, while its recall
    // guard leaves pre-deployment history attributed by the pairing. Runs post-commit because the
    // reprocess opens its own transaction and refreshes continuous aggregates (which can't run
    // inside one).
    let slot_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT DISTINCT sp.site_id, sp.parameter_id
              FROM data_streams ds JOIN site_parameters sp ON ds.site_parameter_id = sp.id
              WHERE ds.pairing_plan_id = $1",
            [plan_id.into()],
        ))
        .await
        .unwrap_or_default();
    let slots: Vec<(Uuid, Uuid)> = slot_rows
        .into_iter()
        .filter_map(|r| {
            let s: Uuid = r.try_get("", "site_id").ok()?;
            let p: Uuid = r.try_get("", "parameter_id").ok()?;
            Some((s, p))
        })
        .collect();
    // Re-derivation runs as tracked jobs so a failure is visible and rerunnable rather than a log
    // line lost on restart.
    for (site_id, parameter_id) in slots {
        crate::routes::private::reprocessing_jobs::worker::enqueue(
            db,
            "pairing_backfill",
            None,
            None,
            &serde_json::json!({ "site_id": site_id, "parameter_id": parameter_id }),
            None,
        )
        .await?;
    }
    crate::routes::private::reprocessing_jobs::worker::enqueue(
        db,
        "refresh_aggregates_full",
        None,
        None,
        &serde_json::json!({ "full": true }),
        None,
    )
    .await?;

    let result = ApplyResult {
        projects_created: counters.projects_created,
        sites_created: counters.sites_created,
        parameters_created: counters.params_created,
        site_parameters_created: counters.sp_created,
        streams_paired: counters.streams_paired,
        streams_skipped: counters.streams_skipped,
        instruments_created: counters.instruments_created,
        curves_assigned: counters.curves_assigned,
        readings_backfilled,
    };

    tracing::info!(
        plan_id = %plan_id,
        streams_paired = counters.streams_paired,
        streams_skipped = counters.streams_skipped,
        sites_created = counters.sites_created,
        params_created = counters.params_created,
        readings_backfilled,
        "Pairing plan applied"
    );

    Ok(result)
}

/// Resolve or create all entities for one plan entry. Returns (site_parameter_id, parameter_id).
async fn resolve_plan_entry<C: ConnectionTrait>(
    txn: &C,
    entry: &PlanEntry,
    source_system: &str,
    caches: &mut EntityCaches,
    counters: &mut ApplyCounters,
) -> AppResult<(Uuid, Uuid)> {
    let project_id = resolve_or_create_project(
        txn,
        &entry.project,
        &mut caches.projects,
        &mut counters.projects_created,
        source_system,
    )
    .await?;
    let site_id = resolve_or_create_site(
        txn,
        &entry.site,
        &mut caches.sites,
        &mut counters.sites_created,
        project_id,
    )
    .await?;
    let parameter_id = resolve_or_create_param(
        txn,
        &entry.parameter,
        entry.original_parameter_name.as_deref(),
        &mut caches.params,
        &mut caches.param_names,
        &mut counters.params_created,
    )
    .await?;
    let site_parameter_id = resolve_or_create_site_param(
        txn,
        site_id,
        parameter_id,
        entry,
        caches,
        &mut counters.sp_created,
    )
    .await?;
    Ok((site_parameter_id, parameter_id))
}

/// The slot an entry pairs into, created when the site has none. The entry's review choices (sd
/// estimator, decimal places) reach an existing slot too, each under its own rule.
async fn resolve_or_create_site_param<C: ConnectionTrait>(
    txn: &C,
    site_id: Uuid,
    parameter_id: Uuid,
    entry: &PlanEntry,
    caches: &mut EntityCaches,
    sp_created: &mut u32,
) -> AppResult<Uuid> {
    let units = entry.parameter.units.as_str();
    let decimal_places = entry.decimal_places;
    // Refused rather than defaulted: the review chose this, and an unrecognised value is a bug in
    // the caller, not a licence to pick a divisor.
    let sd_estimator =
        crate::routes::private::readings::sd_estimator::parse_opt(entry.sd_estimator.as_deref())?;
    let key = (site_id, parameter_id);
    if let Some(&id) = caches.site_params.get(&key) {
        return Ok(id);
    }

    let existing = site_parameters::Entity::find()
        .filter(
            Condition::all()
                .add(site_parameters::Column::SiteId.eq(site_id))
                .add(site_parameters::Column::ParameterId.eq(parameter_id)),
        )
        .one(txn)
        .await?;

    let id = if let Some(existing) = existing {
        // The review's choice reaches a slot that already exists too: pairing into an established
        // slot is exactly when its convention gets settled. An entry that chose nothing leaves
        // whatever the slot already declares.
        if let Some(declared) = sd_estimator
            && existing.sd_estimator.as_deref() != Some(declared)
        {
            let mut active: site_parameters::ActiveModel = existing.clone().into();
            active.sd_estimator = Set(Some(declared.to_string()));
            active.update(txn).await?;
        }
        crate::routes::private::data_streams::service::declare_slot_decimal_places(
            txn,
            existing.id,
            decimal_places,
        )
        .await?;
        existing.id
    } else {
        let id = Uuid::new_v4();
        let mut param_name_val = caches
            .param_names
            .get(&parameter_id)
            .cloned()
            .unwrap_or_default();
        // (site_id, name) is unique; a clash here means the name belongs to a different
        // parameter's slot, so suffix with units (or the parameter code) to disambiguate.
        let name_taken = site_parameters::Entity::find()
            .filter(
                Condition::all()
                    .add(site_parameters::Column::SiteId.eq(site_id))
                    .add(site_parameters::Column::Name.eq(param_name_val.clone())),
            )
            .one(txn)
            .await?
            .is_some();
        if name_taken {
            let suffix = if !units.trim().is_empty() {
                units.trim().to_string()
            } else {
                parameters::Entity::find_by_id(parameter_id)
                    .one(txn)
                    .await?
                    .map(|p| p.code)
                    .unwrap_or_else(|| parameter_id.to_string())
            };
            param_name_val = format!("{param_name_val} ({suffix})");
        }
        let units_val = {
            let u = units.trim();
            (!u.is_empty()).then(|| u.to_string())
        };
        site_parameters::ActiveModel {
            id: Set(id),
            instrument_sensor_id: Set(None),
            site_id: Set(site_id),
            parameter_id: Set(parameter_id),
            name: Set(param_name_val),
            sensor_type: Set(String::new()),
            sd_estimator: Set(sd_estimator.map(str::to_string)),
            display_units: Set(units_val.clone()),
            units_name: Set(units_val),
            units_min: Set(None),
            units_max: Set(None),
            decimal_places: Set(decimal_places),
            channel_id: Set(None),
            sample_interval_sec: Set(None),
            is_active: Set(Some(true)),
            is_public: Set(Some(false)),
            needs_review: Set(false),
            entry_mode: Set("manual".to_string()),
            variable_mappings: Set(None),
            created_at: Set(Some(Utc::now())),
            updated_at: Set(Some(Utc::now())),
            discovered_at: Set(Some(Utc::now())),
        }
        .insert(txn)
        .await?;
        *sp_created += 1;
        id
    };
    caches.site_params.insert(key, id);
    Ok(id)
}

/// Create the lab instruments a plan's confirmed entries ask for, one per `source_key` however
/// many streams share it, and return them by that key. Find-or-create, so re-running an apply
/// after a partial failure resolves the same rows.
async fn mint_plan_instruments<C: ConnectionTrait>(
    txn: &C,
    source_system: &str,
    entries: &[PlanEntry],
) -> AppResult<HashMap<String, Uuid>> {
    let mut wanted: HashMap<&str, &PlanInstrumentRef> = HashMap::new();
    for entry in entries.iter().filter(|e| e.action == "pair") {
        if let Some(i) = &entry.instrument
            && i.create
            && i.id.is_none()
        {
            wanted.entry(i.source_key.as_str()).or_insert(i);
        }
    }

    // Through `upsert_source_instrument` rather than a bare insert: this runs inside `apply_plan`'s
    // transaction, where a unique violation on `sensors_provenance_uniq` from a concurrent apply
    // would poison the whole plan, not just this row.
    let mut minted = HashMap::new();
    for (source_key, want) in wanted {
        let id = upsert_source_instrument(
            txn,
            source_system,
            source_key,
            &want.name,
            // The key is `stream_instrument_key`'s on both sides, so a hand pairing and a plan
            // converge on one row rather than on two that disagree about what it is.
            InstrumentKind::SourceParameter,
            "high",
            None,
        )
        .await?;
        minted.insert(source_key.to_string(), id);
    }
    Ok(minted)
}

async fn pair_entry_stream<C: ConnectionTrait>(
    txn: &C,
    stream: data_streams::Model,
    plan_id: Uuid,
    site_parameter_id: Uuid,
    parameter_id: Uuid,
    instrument_id: Option<Uuid>,
) -> AppResult<()> {
    // The plan's instrument, when the stream does not already name one. A lab instrument gets no
    // deployment: it corrects a grab, it is not stationed at the site, and the "attributed but not
    // deployed" state is the one `import_sensor_for_stream` documents.
    let from_plan = stream
        .sensor_id
        .is_none()
        .then_some(instrument_id)
        .flatten();
    let needs_sensor = stream.sensor_id.is_none() && from_plan.is_none();
    let device =
        crate::routes::private::sensors::identity::extract_vaisala_device_serial(&stream.metadata)
            .is_some();
    // Read once, and only for the entries that will use it: an apply runs this per stream.
    let site_id = if needs_sensor || device {
        site_parameters::Entity::find_by_id(site_parameter_id)
            .one(txn)
            .await?
            .map(|sp| sp.site_id)
            .unwrap_or_default()
    } else {
        Uuid::nil()
    };

    // An entry the review left without an instrument takes the one its own source and parameter
    // resolve, minted here. The apply fails rather than pairing a slot whose readings would name
    // nothing that measured them.
    if needs_sensor {
        create_sensor_for_stream(txn, &stream, parameter_id, site_id).await?;
    } else if device && let Some(sensor_id) = stream.sensor_id.or(from_plan) {
        // A device is stationed at the site whichever route named it, so the slot's deployment is
        // opened here too. Without this the plan's own instrument choice silently costs the
        // deployment that pairing the same stream by hand would have opened.
        let opens_at =
            crate::routes::private::sensors::identity::stream_history_start(txn, stream.id).await?;
        if let Err(e) = crate::routes::private::sensors::identity::find_or_create_deployment(
            txn,
            sensor_id,
            site_id,
            parameter_id,
            opens_at,
        )
        .await
        {
            tracing::warn!(
                error = %e,
                stream_id = %stream.id,
                %sensor_id,
                "Failed to open the deployment for a device-shaped stream during pairing",
            );
        }
    }

    let now = Utc::now();
    let mut active: data_streams::ActiveModel = stream.into();
    // Only assign when the plan resolved it. `create_sensor_for_stream` links the stream itself,
    // and this model predates that write, so setting the field unconditionally would clobber it.
    if let Some(id) = from_plan {
        active.sensor_id = Set(Some(id));
    }
    active.site_parameter_id = Set(Some(site_parameter_id));
    active.pairing_plan_id = Set(Some(plan_id));
    active.paired_at = Set(Some(now.into()));
    active.updated_at = Set(now.into());
    active.update(txn).await?;
    Ok(())
}

/// Rows the plan's readings point at through `column`, read before the readings lose it.
async fn plan_reading_references<C: ConnectionTrait>(
    conn: &C,
    plan_id: Uuid,
    column: &str,
) -> AppResult<Vec<Uuid>> {
    Ok(conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT DISTINCT r.{column} AS id FROM readings r
                 JOIN data_streams ds ON r.stream_id = ds.id
                 WHERE ds.pairing_plan_id = $1 AND r.{column} IS NOT NULL"
            ),
            [plan_id.into()],
        ))
        .await?
        .iter()
        .filter_map(|row| row.try_get::<Uuid>("", "id").ok())
        .collect())
}

/// Attribute everything the plan's newly paired streams already hold, through the helper every
/// pairing path runs. Deployment attribution is left to the slot reprocess the caller enqueues:
/// a plan pairs many streams, and each reading's deployment is the one covering its own time.
async fn backfill_plan_readings<C: ConnectionTrait>(
    txn: &C,
    plan_id: Uuid,
) -> AppResult<crate::routes::private::data_streams::pairing::Backfilled> {
    crate::routes::private::data_streams::pairing::backfill(
        txn,
        crate::routes::private::sync::replicate_audit::HoldScope::Plan(plan_id),
        None,
    )
    .await
}

async fn finalize_plan<C: ConnectionTrait>(
    txn: &C,
    plan_id: Uuid,
    counters: &ApplyCounters,
    readings_backfilled: u64,
) -> AppResult<()> {
    let result = ApplyResult {
        projects_created: counters.projects_created,
        sites_created: counters.sites_created,
        parameters_created: counters.params_created,
        site_parameters_created: counters.sp_created,
        streams_paired: counters.streams_paired,
        streams_skipped: counters.streams_skipped,
        instruments_created: counters.instruments_created,
        curves_assigned: counters.curves_assigned,
        readings_backfilled,
    };

    let mut plan_active: pairing_plans::ActiveModel = pairing_plans::Entity::find_by_id(plan_id)
        .one(txn)
        .await?
        .ok_or_else(|| AppError::Internal("Plan disappeared during apply".to_string()))?
        .into();
    plan_active.status = Set("applied".to_string());
    plan_active.applied_at = Set(Some(Utc::now().into()));
    plan_active.apply_result = Set(Some(serde_json::to_value(&result).unwrap_or_default()));
    plan_active.update(txn).await?;
    Ok(())
}

/// Revert a pairing plan: bulk unpair all streams that were paired by this plan.
pub async fn revert_plan(db: &sea_orm::DatabaseConnection, plan_id: Uuid) -> AppResult<u32> {
    let plan = pairing_plans::Entity::find_by_id(plan_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    if plan.status != "applied" {
        return Err(AppError::BadRequest(format!(
            "Plan is '{}', can only revert 'applied' plans",
            plan.status
        )));
    }

    let txn = db.begin().await?;

    // Atomic status claim: a concurrent revert of the same plan matches zero rows and bails.
    let claimed = txn
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE pairing_plans SET status = 'reverting' WHERE id = $1 AND status = 'applied'",
            [plan_id.into()],
        ))
        .await?;
    if claimed.rows_affected() == 0 {
        return Err(AppError::BadRequest(
            "Plan is no longer in applied status".to_string(),
        ));
    }

    crate::common::bulk_write::lift_decompression_cap(&txn).await?;

    // NULL out readings for streams from this plan; samples formed by the pairing backfill
    // lose their last reference and are removed below
    // Samples referenced by this plan's readings, so only those can be removed below.
    let sample_ids = plan_reading_references(&txn, plan_id, "sample_id").await?;
    // The visit is attributed state too: `collection_events::attach` only stamps a reading whose
    // collection_event_id is NULL, so a reading left pointing at the reverted site's visit would
    // never be re-attached when the stream is paired somewhere else.
    let event_ids = plan_reading_references(&txn, plan_id, "collection_event_id").await?;

    txn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE readings r
          SET site_id = NULL, parameter_id = NULL, sample_id = NULL, collection_event_id = NULL
          FROM data_streams ds
          WHERE r.stream_id = ds.id AND ds.pairing_plan_id = $1",
        [plan_id.into()],
    ))
    .await?;

    // Reverting the pairing takes the reviewer away again; open reviews wait as deferred.
    crate::routes::private::sync::replicate_audit::repoint_holds(
        &txn,
        crate::routes::private::sync::replicate_audit::HoldScope::Plan(plan_id),
        false,
    )
    .await?;

    if !sample_ids.is_empty() {
        txn.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"DELETE FROM samples s
              WHERE s.id = ANY($1)
                AND NOT EXISTS (SELECT 1 FROM readings r WHERE r.sample_id = s.id)",
            [sample_ids.into()],
        ))
        .await?;
    }

    if !event_ids.is_empty() {
        txn.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"DELETE FROM collection_events ce
              WHERE ce.id = ANY($1)
                AND NOT EXISTS (SELECT 1 FROM readings r WHERE r.collection_event_id = ce.id)",
            [event_ids.into()],
        ))
        .await?;
    }

    txn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE status_events se SET site_id = NULL, parameter_id = NULL
          FROM data_streams ds
          WHERE se.stream_id = ds.id AND ds.pairing_plan_id = $1",
        [plan_id.into()],
    ))
    .await?;

    // Unpair the streams; pairing_plan_id stays as the audit link back to this plan
    let result = txn
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE data_streams SET site_parameter_id = NULL, paired_at = NULL
          WHERE pairing_plan_id = $1",
            [plan_id.into()],
        ))
        .await?;
    let reverted = result.rows_affected() as u32;

    // Update plan status
    let mut plan_active: pairing_plans::ActiveModel = pairing_plans::Entity::find_by_id(plan_id)
        .one(&txn)
        .await?
        .ok_or_else(|| AppError::Internal("Plan disappeared during revert".to_string()))?
        .into();
    plan_active.status = Set("reverted".to_string());
    plan_active.update(&txn).await?;

    txn.commit().await?;

    // Refresh aggregates synchronously so callers see consistent state
    crate::common::sync_state::refresh_continuous_aggregates_full(db).await?;

    tracing::info!(plan_id = %plan_id, reverted, "Pairing plan reverted");
    Ok(reverted)
}

/// How much attention one entry still wants, the three states the review renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewState {
    /// Project, site and parameter all resolve and nothing warned, so the proposal stands on its
    /// own evidence. Worth looking over, not waiting on anyone.
    SelfValidated,
    /// Something did not resolve, or the entry carries a warning. A person decides this one.
    NeedsChecking,
    /// A person looked and agreed.
    Acknowledged,
}

/// The state of one entry, most decided first.
#[must_use]
pub fn review_state(entry: &PlanEntry) -> ReviewState {
    if entry.acknowledged {
        return ReviewState::Acknowledged;
    }
    if entry.confidence == "exact" && entry.warnings.is_empty() {
        return ReviewState::SelfValidated;
    }
    ReviewState::NeedsChecking
}

pub fn compute_summary_pub(entries: &[PlanEntry]) -> PlanSummary {
    compute_summary(entries)
}

fn compute_summary(entries: &[PlanEntry]) -> PlanSummary {
    let will_pair = entries.iter().filter(|e| e.action == "pair").count();
    let will_skip = entries.iter().filter(|e| e.action == "skip").count();

    let unique_projects: std::collections::HashSet<&str> = entries
        .iter()
        .filter(|e| e.action == "pair")
        .map(|e| e.project.name.as_str())
        .collect();
    let unique_sites: std::collections::HashSet<&str> = entries
        .iter()
        .filter(|e| e.action == "pair")
        .map(|e| e.site.name.as_str())
        .collect();
    let unique_params: std::collections::HashSet<&str> = entries
        .iter()
        .filter(|e| e.action == "pair")
        .map(|e| e.parameter.name.as_str())
        .collect();

    let projects_to_create = entries
        .iter()
        .filter(|e| e.action == "pair" && e.project.create)
        .map(|e| &e.project.name)
        .collect::<std::collections::HashSet<_>>()
        .len();
    let sites_to_create = entries
        .iter()
        .filter(|e| e.action == "pair" && e.site.create)
        .map(|e| &e.site.name)
        .collect::<std::collections::HashSet<_>>()
        .len();
    let params_to_create = entries
        .iter()
        .filter(|e| e.action == "pair" && e.parameter.create)
        .map(|e| &e.parameter.name)
        .collect::<std::collections::HashSet<_>>()
        .len();

    // Instruments are counted by identity, not by entry: one curve column serves every station in
    // the source, so 31 DOC streams create at most one instrument.
    let instruments_to_create = entries
        .iter()
        .filter(|e| e.action == "pair")
        .filter_map(|e| e.instrument.as_ref())
        .filter(|i| i.create)
        .map(|i| &i.source_key)
        .collect::<std::collections::HashSet<_>>()
        .len();
    let instruments_unconfirmed = entries
        .iter()
        .filter(|e| e.action == "pair")
        .filter_map(|e| e.instrument.as_ref())
        .filter(|i| i.create && !i.confirmed)
        .map(|i| &i.source_key)
        .collect::<std::collections::HashSet<_>>()
        .len();

    let pairing = entries.iter().filter(|e| e.action == "pair");
    let mut needs_checking = 0usize;
    let mut self_validated = 0usize;
    let mut acknowledged = 0usize;
    for entry in pairing {
        match review_state(entry) {
            ReviewState::NeedsChecking => needs_checking += 1,
            ReviewState::SelfValidated => self_validated += 1,
            ReviewState::Acknowledged => acknowledged += 1,
        }
    }

    PlanSummary {
        total_streams: entries.len(),
        will_pair,
        needs_checking,
        self_validated,
        acknowledged,
        will_skip,
        projects_to_create,
        sites_to_create,
        parameters_to_create: params_to_create,
        instruments_to_create,
        instruments_unconfirmed,
        unique_projects: unique_projects.len(),
        unique_sites: unique_sites.len(),
        unique_parameters: unique_params.len(),
    }
}

fn match_entity(name: &str, existing: &[(Uuid, String)]) -> (Option<Uuid>, bool) {
    if name.is_empty() {
        return (None, false);
    }
    let lower = name.to_lowercase();
    if let Some((id, _)) = existing.iter().find(|(_, n)| n.to_lowercase() == lower) {
        (Some(*id), false)
    } else {
        (None, true)
    }
}

/// A stream names its column by code, display name or alias, so all three resolve. The order is
/// canonical: `resolve_or_create_param` runs it as SQL at apply time and this builds the same
/// precedence into the review's lookup map, so a review shows what apply will produce.
pub fn lookup_parameter_by_code_name_or_alias(
    name: &str,
    existing: &[CatalogParam],
) -> Option<Uuid> {
    if name.is_empty() {
        return None;
    }
    let lower = name.to_lowercase();
    existing
        .iter()
        .find(|p| p.code.to_lowercase() == lower)
        .or_else(|| existing.iter().find(|p| p.name.to_lowercase() == lower))
        .or_else(|| {
            existing
                .iter()
                .find(|p| p.aliases.iter().any(|a| a.to_lowercase() == lower))
        })
        .map(|p| p.id)
}

/// The parameter a replicate family should suggest: the measurand, not the incoming statistic
/// column. Strips the `avg` marker (`DOC_avg_ppb` -> `DOC_ppb`), and when dropping a trailing
/// token on top of that finds an existing catalog parameter (`DOC_ppb` -> `DOC`), prefers it, so
/// a synced family and a tool save land on one slot instead of minting a sibling.
/// The catalog code a replicate family's mean column suggests.
///
/// The incoming column header is the code, because that is how the data is already stored, so
/// nothing is stripped from it except `avg`: an `_avg` column is by construction the mean of a
/// replicate family, which makes that segment structural rather than a suffix, and the family is
/// what is being paired. `DOC_avg_ppb` is `DOC_ppb`, units and all; a units-bearing column never
/// resolves onto a shorter code, so a catalog that happens to hold `DOC` does not pull `DOC_ppb`
/// onto it and give two portals different export headers for the same measurand.
fn family_parameter_suggestion(name: &str) -> String {
    let stripped: String = name
        .split('_')
        .filter(|seg| !seg.eq_ignore_ascii_case("avg"))
        .collect::<Vec<_>>()
        .join("_");
    if stripped.is_empty() {
        return name.to_string();
    }
    stripped
}

fn match_entity_display(name: &str, existing: &[CatalogParam]) -> (Option<Uuid>, bool) {
    if name.is_empty() {
        return (None, false);
    }
    match lookup_parameter_by_code_name_or_alias(name, existing) {
        Some(id) => (Some(id), false),
        None => (None, true),
    }
}

pub struct CatalogParam {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub units: String,
    pub category: String,
    /// What already depends on this parameter. A catalog entry nothing uses is a different
    /// proposition from one carrying years of readings, and a units conflict cannot be judged
    /// without knowing which it is.
    pub site_parameter_count: i64,
    pub reading_count: i64,
}

pub struct EntityCatalog {
    pub projects: Vec<(Uuid, String)>,
    pub sites: Vec<(Uuid, String)>,
    pub params: Vec<CatalogParam>,
}

pub async fn load_entity_catalog(db: &impl ConnectionTrait) -> AppResult<EntityCatalog> {
    let projects = projects::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();
    let sites = sites::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.id, s.name))
        .collect();
    // Usage per parameter in one pass. `readings.parameter_id` is indexed and the group-by is over
    // the slots, not the hypertable's rows, so this stays a catalog-sized query.
    let mut usage: HashMap<Uuid, (i64, i64)> = HashMap::new();
    for row in db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sp.parameter_id AS parameter_id,
                    COUNT(*) AS slots,
                    COALESCE(SUM(r.n), 0) AS readings
             FROM site_parameters sp
             LEFT JOIN (
                 SELECT site_id, parameter_id, COUNT(*) AS n
                 FROM readings WHERE parameter_id IS NOT NULL
                 GROUP BY site_id, parameter_id
             ) r ON r.parameter_id = sp.parameter_id AND r.site_id = sp.site_id
             GROUP BY sp.parameter_id"
                .to_owned(),
        ))
        .await?
    {
        let id: Uuid = row.try_get("", "parameter_id")?;
        let slots: i64 = row.try_get("", "slots").unwrap_or(0);
        let readings: i64 = row.try_get("", "readings").unwrap_or(0);
        usage.insert(id, (slots, readings));
    }

    let params = parameters::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|p| {
            let (slots, readings) = usage.get(&p.id).copied().unwrap_or((0, 0));
            CatalogParam {
                id: p.id,
                code: p.code,
                name: p.name,
                aliases: p.aliases,
                units: p.default_units,
                category: p.category,
                site_parameter_count: slots,
                reading_count: readings,
            }
        })
        .collect();
    Ok(EntityCatalog {
        projects,
        sites,
        params,
    })
}

/// Recompute an entry's entity resolution against the current catalog: project/site/parameter
/// id + create flags, unit-mismatch warnings, and overall confidence. Warnings are rebuilt from
/// scratch so ones that no longer apply are cleared. Does not touch action or grouping fields.
pub fn reclassify_entry(entry: &mut PlanEntry, catalog: &EntityCatalog) {
    let (proj_id, proj_create) = match_entity(&entry.project.name, &catalog.projects);
    entry.project.id = proj_id;
    entry.project.create = proj_create;

    let (site_id, site_create) = match_entity(&entry.site.name, &catalog.sites);
    entry.site.id = site_id;
    entry.site.create = site_create;

    let (param_id, param_create) = match_entity_display(&entry.parameter.name, &catalog.params);
    entry.parameter.id = param_id;
    entry.parameter.create = param_create;

    entry.warnings.clear();
    if let Some(pid) = param_id
        && let Some(p) = catalog.params.iter().find(|p| p.id == pid)
        && !p.units.is_empty()
        && !entry.parameter.units.is_empty()
        && p.units.to_lowercase() != entry.parameter.units.to_lowercase()
    {
        entry.warnings.push(PlanWarning::units_mismatch(
            &entry.parameter.name,
            p,
            &entry.parameter.units,
        ));
    }
    // A family whose source reports an sd and has no declaration yet. `catalog` has no slot rows,
    // so this reads the plan's own declaration: an entry that has already been patched with one,
    // or adopted its slot's, is settled.
    if entry.sd_estimator.is_none()
        && entry
            .replicates
            .as_ref()
            .is_some_and(|r| r.portal_sd_column.is_some())
    {
        entry.warnings.push(PlanWarning::sd_estimator_undeclared(
            &entry.parameter.name,
            entry.sd_population_holds,
        ));
    }

    entry.confidence = if proj_id.is_some() && site_id.is_some() && param_id.is_some() {
        "exact"
    } else {
        "none"
    }
    .to_string();
}

use sea_orm::sea_query::Expr;

async fn resolve_or_create_project<C: ConnectionTrait>(
    txn: &C,
    entity_ref: &PlanEntityRef,
    cache: &mut HashMap<String, Uuid>,
    created_count: &mut u32,
    source_system: &str,
) -> AppResult<Uuid> {
    if let Some(id) = entity_ref.id {
        return Ok(id);
    }
    let key = entity_ref.name.to_lowercase();
    if let Some(&id) = cache.get(&key) {
        return Ok(id);
    }
    let existing = projects::Entity::find()
        .filter(Expr::cust_with_values("LOWER(name) = $1", [key.clone()]))
        .one(txn)
        .await?;
    if let Some(existing) = existing {
        cache.insert(key, existing.id);
        return Ok(existing.id);
    }
    let id = Uuid::new_v4();
    projects::ActiveModel {
        id: Set(id),
        name: Set(entity_ref.name.clone()),
        description: Set(None),
        data_source: Set(Some(source_system.to_string())),
        is_public: Set(false),
        public_code: Set(None),
        public_api_title: Set(None),
        public_api_description: Set(None),
        public_api_version: Set(None),
        public_contact_email: Set(None),
        created_at: Set(Some(Utc::now())),
        discovered_at: Set(Some(Utc::now())),
    }
    .insert(txn)
    .await?;
    *created_count += 1;
    cache.insert(key, id);
    Ok(id)
}

async fn resolve_or_create_site(
    txn: &impl ConnectionTrait,
    site_ref: &PlanSiteRef,
    cache: &mut HashMap<String, Uuid>,
    created_count: &mut u32,
    project_id: Uuid,
) -> AppResult<Uuid> {
    if let Some(id) = site_ref.id {
        // The site was matched at plan-creation time. Still backfill coordinates from the stream
        // metadata if the site lacks them, otherwise a site discovered before its coordinates were
        // known never picks them up (the common case, since match_entity sets the id).
        if site_ref.latitude.is_some()
            && let Some(existing) = sites::Entity::find_by_id(id).one(txn).await?
            && existing.latitude.is_none()
        {
            let mut update: sites::ActiveModel = existing.into();
            update.latitude = Set(site_ref.latitude);
            update.longitude = Set(site_ref.longitude);
            update.altitude_m = Set(site_ref.altitude_m);
            update.update(txn).await?;
        }
        return Ok(id);
    }
    let key = site_ref.name.to_lowercase();
    if let Some(&id) = cache.get(&key) {
        return Ok(id);
    }
    let existing = sites::Entity::find()
        .filter(Expr::cust_with_values("LOWER(name) = $1", [key.clone()]))
        .one(txn)
        .await?;
    if let Some(existing) = existing {
        if existing.latitude.is_none() && site_ref.latitude.is_some() {
            let mut update: sites::ActiveModel = existing.clone().into();
            update.latitude = Set(site_ref.latitude);
            update.longitude = Set(site_ref.longitude);
            update.altitude_m = Set(site_ref.altitude_m);
            update.update(txn).await?;
        }
        cache.insert(key, existing.id);
        return Ok(existing.id);
    }
    let id = Uuid::new_v4();
    sites::ActiveModel {
        id: Set(id),
        project_id: Set(Some(project_id)),
        subproject_id: sea_orm::ActiveValue::NotSet,
        name: Set(site_ref.name.clone()),
        latitude: Set(site_ref.latitude),
        longitude: Set(site_ref.longitude),
        altitude_m: Set(site_ref.altitude_m),
        public_code: Set(None),
        meteoswiss_station_abbr: sea_orm::ActiveValue::NotSet,
        created_at: Set(Some(Utc::now())),
        discovered_at: Set(Some(Utc::now())),
    }
    .insert(txn)
    .await?;
    *created_count += 1;
    cache.insert(key, id);
    Ok(id)
}

async fn resolve_or_create_param(
    txn: &impl ConnectionTrait,
    param_ref: &PlanParamRef,
    original_parameter_name: Option<&str>,
    cache: &mut HashMap<String, Uuid>,
    param_names: &mut HashMap<Uuid, String>,
    created_count: &mut u32,
) -> AppResult<Uuid> {
    if let Some(id) = param_ref.id {
        return Ok(id);
    }
    let key = param_ref.name.to_lowercase();
    if let Some(&id) = cache.get(&key) {
        return Ok(id);
    }
    // Resolution order mirrors `match_entity_display`: code, then name, then alias,
    // all case-insensitive.
    let existing = parameters::Entity::find()
        .filter(Expr::cust_with_values("LOWER(code) = $1", [key.clone()]))
        .one(txn)
        .await?;
    if let Some(existing) = existing {
        cache.insert(key, existing.id);
        param_names.entry(existing.id).or_insert(existing.name);
        return Ok(existing.id);
    }
    let name_match = parameters::Entity::find()
        .filter(Expr::cust_with_values("LOWER(name) = $1", [key.clone()]))
        .one(txn)
        .await?;
    if let Some(matched) = name_match {
        cache.insert(key, matched.id);
        param_names.entry(matched.id).or_insert(matched.name);
        return Ok(matched.id);
    }
    let alias_match = parameters::Entity::find()
        .filter(Expr::cust_with_values(
            "EXISTS (SELECT 1 FROM unnest(aliases) a WHERE LOWER(a) = $1)",
            [key.clone()],
        ))
        .one(txn)
        .await?;
    if let Some(matched) = alias_match {
        cache.insert(key, matched.id);
        param_names.entry(matched.id).or_insert(matched.name);
        return Ok(matched.id);
    }
    // No match: create. The column name is the code (the stable machine id a scientist can match
    // against the portal's own tables), the label is the human name, and both plus the source
    // names seed the aliases so future plans resolve any of them.
    let mut aliases: Vec<String> = param_ref
        .original_names
        .iter()
        .cloned()
        .chain(original_parameter_name.map(str::to_string))
        .chain(param_ref.label.clone())
        .filter(|a| !a.trim().is_empty() && a.to_lowercase() != key)
        .collect();
    aliases.sort();
    aliases.dedup_by(|a, b| a.to_lowercase() == b.to_lowercase());
    let category = infer_category(&param_ref.name);
    let id = Uuid::new_v4();
    parameters::ActiveModel {
        id: Set(id),
        code: Set(param_ref.name.clone()),
        name: Set(param_ref
            .label
            .clone()
            .unwrap_or_else(|| param_ref.name.clone())),
        default_units: Set(param_ref.units.clone()),
        category: Set(category),
        // Mechanically created from a sync source; a manager confirms or merges it later.
        needs_review: Set(true),
        description: Set(None),
        aliases: Set(aliases),
        created_at: Set(Some(Utc::now())),
    }
    .insert(txn)
    .await?;
    *created_count += 1;
    cache.insert(key, id);
    param_names.insert(id, param_ref.name.clone());
    Ok(id)
}

fn infer_category(_name: &str) -> String {
    "measurement".to_string()
}

/// Which entries a plan-wide bulk action covers. Every field is a further narrowing, so an empty
/// `BulkWhere` selects the whole plan; a plan-wide action is then one predicate on the wire rather
/// than one update per entry (1891 for CNET, 29,400 for NOMIS).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BulkWhere {
    /// `exact` when the project, site and parameter all resolved, `none` otherwise.
    #[serde(default)]
    pub confidence: Option<String>,
    #[serde(default)]
    pub has_warnings: Option<bool>,
    #[serde(default)]
    pub site_name: Option<String>,
    #[serde(default)]
    pub parameter_name: Option<String>,
}

/// The positions in `entries` the predicate picks.
pub fn select_entries(entries: &[PlanEntry], filter: &BulkWhere) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            filter
                .confidence
                .as_deref()
                .is_none_or(|c| e.confidence.eq_ignore_ascii_case(c))
                && filter
                    .has_warnings
                    .is_none_or(|w| e.warnings.is_empty() != w)
                && filter
                    .site_name
                    .as_deref()
                    .is_none_or(|n| e.site.name.eq_ignore_ascii_case(n))
                && filter
                    .parameter_name
                    .as_deref()
                    .is_none_or(|n| e.parameter.name.eq_ignore_ascii_case(n))
        })
        .map(|(i, _)| i)
        .collect()
}

/// Apply a bulk action to the selected entries. An entry with no site or no parameter name is
/// never set to `pair`: there is no slot to pair it to, which is the rule the per-entry updates
/// already enforce.
pub fn apply_bulk_action(entries: &mut [PlanEntry], filter: &BulkWhere, action: &str) -> usize {
    let selected = select_entries(entries, filter);
    let mut changed = 0;
    for i in selected {
        let entry = &mut entries[i];
        let target = if action == "pair"
            && (entry.site.name.trim().is_empty() || entry.parameter.name.trim().is_empty())
        {
            "skip"
        } else {
            action
        };
        if entry.action != target {
            entry.action = target.to_string();
            changed += 1;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::{
        BulkWhere, InstrumentCatalog, PlanEntry, apply_bulk_action, family_parameter_suggestion,
        resolve_parameter_instrument, select_entries, stream_instrument_key,
    };
    use std::collections::HashMap;
    use uuid::Uuid;

    fn catalog(entries: &[(&str, Uuid)]) -> InstrumentCatalog {
        InstrumentCatalog {
            by_id: entries
                .iter()
                .map(|(key, id)| (*id, ((*key).to_string(), Some((*key).to_string()))))
                .collect(),
            labels: vec![],
            by_source_key: entries
                .iter()
                .map(|(key, id)| ((*key).to_string(), *id))
                .collect(),
            by_name: HashMap::new(),
            curves: HashMap::new(),
        }
    }

    /// A catalog holding one instrument by name and nothing else, for the collision cases.
    fn catalog_named(name: &str, id: Uuid, has_readings: bool) -> InstrumentCatalog {
        let mut c = catalog(&[]);
        c.by_name.insert(
            name.to_lowercase(),
            super::InstrumentNameConflict {
                id,
                name: name.to_string(),
                source_system: Some("metalp".to_string()),
                has_readings,
            },
        );
        c
    }

    fn family_stream() -> super::data_streams::Model {
        let now = chrono::Utc::now().into();
        super::data_streams::Model {
            id: Uuid::new_v4(),
            source_system: "cnet".to_string(),
            source_key: "FP3:DOC_avg_ppb:reps".to_string(),
            source_name: None,
            source_path: None,
            metadata: serde_json::json!({
                "hierarchy": { "project": "CNET", "site": "FP3", "parameter": "DOC_avg_ppb" }
            }),
            site_parameter_id: None,
            sensor_id: None,
            measurement_type: None,
            is_active: true,
            discovered_at: now,
            paired_at: None,
            last_data_time: None,
            last_window_digest: None,
            pairing_plan_id: None,
            created_at: now,
            updated_at: now,
            replicates: None,
        }
    }

    /// The plan proposes the instrument the pairing mints, for a replicate family too: the
    /// suggested parameter is a label, and keying the proposal on it mints a second row for the
    /// same analyte.
    #[test]
    fn test_the_plan_proposes_the_key_the_pairing_mints() {
        let stream = family_stream();
        let suggestion = family_parameter_suggestion("DOC_avg_ppb");
        assert_ne!(suggestion, "DOC_avg_ppb", "the suggestion is a label");

        let proposed = resolve_parameter_instrument(
            stream_instrument_key(&stream),
            &suggestion,
            &catalog(&[]),
        );

        assert_eq!(proposed.source_key, "cnet:DOC_avg_ppb");
    }

    #[test]
    fn test_resolve_parameter_instrument_takes_the_source_s_own() {
        let id = Uuid::new_v4();
        let resolved = resolve_parameter_instrument(
            "cnet:NO2_mgL".to_string(),
            "NO2_mgL",
            &catalog(&[("cnet:NO2_mgL", id)]),
        );
        assert_eq!(resolved.id, Some(id));
        assert!(
            !resolved.create,
            "an instrument that exists is not created again"
        );
        assert!(resolved.confirmed);
    }

    /// Expected behaviour: a parameter with no instrument is proposed, already agreed. The review
    /// changes it by attaching another; leaving it alone creates the suggestion.
    #[test]
    fn test_resolve_parameter_instrument_proposes_one_already_agreed() {
        let proposed =
            resolve_parameter_instrument("cnet:NO2_mgL".to_string(), "NO2_mgL", &catalog(&[]));
        assert_eq!(proposed.id, None);
        assert_eq!(proposed.source_key, "cnet:NO2_mgL");
        assert!(proposed.create && proposed.confirmed);
        assert_eq!(
            proposed.proposed_name.as_deref(),
            Some("NO2_mgL"),
            "the name is the analyte; the source is provenance and lives in source_key"
        );
        assert_eq!(proposed.name, "NO2_mgL");
        assert!(proposed.name_conflict.is_none());
    }

    /// Expected behaviour: the lab's DOC analyser is one machine carried to every station, so a
    /// proposal that would create a second instrument called `DOC` is a decision, not a
    /// suggestion. It is reported unconfirmed with the row it collides with, and apply refuses an
    /// unconfirmed proposal, so the operator has to say which they meant.
    #[test]
    fn test_a_proposed_name_an_instrument_already_carries_is_put_to_the_operator() {
        let existing = Uuid::new_v4();
        let proposed = resolve_parameter_instrument(
            "cnet:DOC".to_string(),
            "DOC",
            &catalog_named("DOC", existing, true),
        );
        assert!(
            proposed.create,
            "attaching is one of the two answers, not the default"
        );
        assert!(
            !proposed.confirmed,
            "a collision is never agreed to on the operator's behalf"
        );
        let conflict = proposed.name_conflict.expect("the collision is reported");
        assert_eq!(conflict.id, existing);
        assert_eq!(conflict.name, "DOC");
        assert!(
            conflict.has_readings,
            "attaching would add to readings it already holds, which is what must be said"
        );
    }

    /// The comparison is on the name a person reads, so case and surrounding space are not a
    /// second instrument.
    #[test]
    fn test_a_collision_ignores_case_and_padding() {
        let existing = Uuid::new_v4();
        let proposed = resolve_parameter_instrument(
            "cnet:doc".to_string(),
            "  doc  ",
            &catalog_named("DOC", existing, false),
        );
        assert_eq!(
            proposed.name_conflict.map(|c| c.id),
            Some(existing),
            "`doc` and `DOC` are one instrument to the person choosing"
        );
    }

    pub fn plan_entry(site: &str, parameter: &str, confidence: &str, warnings: usize) -> PlanEntry {
        let entry = serde_json::json!({
            "stream_id": Uuid::new_v4(),
            "source_key": format!("{site}:{parameter}"),
            "source_name": null,
            "action": "pair",
            "project": { "id": null, "name": "CNET", "create": true },
            "site": { "id": null, "name": site, "create": true,
                      "latitude": null, "longitude": null, "altitude_m": null },
            "parameter": { "id": null, "name": parameter, "label": null, "create": true,
                           "units": "mm", "group_key": null, "original_names": [] },
            "confidence": confidence,
            "warnings": (0..warnings)
                .map(|i| serde_json::json!({ "kind": "units_mismatch", "message": i.to_string() }))
                .collect::<Vec<_>>(),
        });
        serde_json::from_value(entry).expect("a plan entry")
    }

    #[test]
    fn test_select_entries_picks_exactly_each_predicate_s_set() {
        let entries = vec![
            plan_entry("FP1", "Depth", "exact", 0),
            plan_entry("FP1", "CDOM", "none", 1),
            plan_entry("FP2", "Depth", "none", 0),
        ];

        assert_eq!(
            select_entries(&entries, &BulkWhere::default()),
            vec![0, 1, 2]
        );
        assert_eq!(
            select_entries(
                &entries,
                &BulkWhere {
                    confidence: Some("none".into()),
                    ..Default::default()
                }
            ),
            vec![1, 2]
        );
        assert_eq!(
            select_entries(
                &entries,
                &BulkWhere {
                    has_warnings: Some(true),
                    ..Default::default()
                }
            ),
            vec![1]
        );
        assert_eq!(
            select_entries(
                &entries,
                &BulkWhere {
                    site_name: Some("fp1".into()),
                    ..Default::default()
                }
            ),
            vec![0, 1],
            "the site is matched case-insensitively, as the review renders it"
        );
        assert_eq!(
            select_entries(
                &entries,
                &BulkWhere {
                    confidence: Some("none".into()),
                    parameter_name: Some("Depth".into()),
                    ..Default::default()
                }
            ),
            vec![2],
            "predicates narrow together"
        );
    }

    #[test]
    fn test_apply_bulk_action_never_pairs_an_entry_with_no_slot() {
        let mut entries = vec![
            plan_entry("", "Depth", "none", 0),
            plan_entry("FP1", "", "none", 0),
            plan_entry("FP1", "Depth", "none", 0),
        ];
        for entry in &mut entries {
            entry.action = "skip".to_string();
        }

        let changed = apply_bulk_action(&mut entries, &BulkWhere::default(), "pair");
        assert_eq!(changed, 1, "only the entry that names a slot moves");
        assert_eq!(entries[0].action, "skip");
        assert_eq!(entries[1].action, "skip");
        assert_eq!(entries[2].action, "pair");

        let changed = apply_bulk_action(&mut entries, &BulkWhere::default(), "skip");
        assert_eq!(changed, 1, "and skipping is its inverse");
        assert!(entries.iter().all(|e| e.action == "skip"));
    }
}

#[cfg(test)]
mod review_state_tests {
    use super::tests::plan_entry;
    use super::{ReviewState, compute_summary, review_state};

    #[test]
    fn test_review_state_asks_only_where_the_evidence_is_short() {
        let matched = plan_entry("FP1", "Depth", "exact", 0);
        assert_eq!(review_state(&matched), ReviewState::SelfValidated);

        let warned = plan_entry("FP1", "CDOM", "exact", 1);
        assert_eq!(review_state(&warned), ReviewState::NeedsChecking);

        let unmatched = plan_entry("FP2", "Depth", "none", 0);
        assert_eq!(review_state(&unmatched), ReviewState::NeedsChecking);

        // A tick settles the entry whatever its evidence said.
        let mut acknowledged = plan_entry("FP2", "CDOM", "none", 2);
        acknowledged.acknowledged = true;
        assert_eq!(review_state(&acknowledged), ReviewState::Acknowledged);
    }

    #[test]
    fn test_compute_summary_counts_the_three_states_over_pairing_entries_only() {
        let mut skipped = plan_entry("FP3", "Depth", "none", 1);
        skipped.action = "skip".to_string();
        let mut acknowledged = plan_entry("FP2", "CDOM", "none", 1);
        acknowledged.acknowledged = true;

        let summary = compute_summary(&[
            plan_entry("FP1", "Depth", "exact", 0),
            plan_entry("FP1", "CDOM", "exact", 0),
            plan_entry("FP2", "Depth", "none", 0),
            acknowledged,
            skipped,
        ]);

        assert_eq!(summary.will_pair, 4);
        assert_eq!(summary.self_validated, 2);
        assert_eq!(summary.needs_checking, 1);
        assert_eq!(summary.acknowledged, 1);
        assert_eq!(
            summary.self_validated + summary.needs_checking + summary.acknowledged,
            summary.will_pair,
            "every pairing entry is in exactly one state"
        );
    }
}

#[cfg(test)]
mod family_suggestion_tests {
    use super::family_parameter_suggestion;

    #[test]
    fn test_family_suggestion_strips_only_the_structural_avg_segment() {
        assert_eq!(family_parameter_suggestion("DOC_avg_ppb"), "DOC_ppb");
        assert_eq!(family_parameter_suggestion("NO2_avg_mgL"), "NO2_mgL");
        assert_eq!(family_parameter_suggestion("avg"), "avg");
    }

    #[test]
    fn test_a_units_bearing_column_never_resolves_onto_a_shorter_code() {
        // The suggestion no longer reads the catalog at all: a catalog holding `DOC` is not a
        // reason to export a `DOC` header where the portal wrote `DOC_avg_ppb`.
        assert_eq!(family_parameter_suggestion("DOC_ppb"), "DOC_ppb");
        assert_eq!(family_parameter_suggestion("DOC"), "DOC");
    }
}
