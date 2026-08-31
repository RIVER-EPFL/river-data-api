use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, EntityTrait, QueryFilter,
    QueryOrder, Set, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::routes::private::sensors::operations::create_sensor_for_stream;
use crate::routes::private::{
    data_streams, data_streams::pairing_plans, parameters, projects, sensors,
    sensors::standard_curves, sites, sites::parameters as site_parameters,
};

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
    pub confidence: String, // "exact" | "fuzzy" | "none"
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
    /// The evidence for that choice: open replicate-statistics holds on this stream, and how many
    /// of them match the population signature. Written at plan creation so the review shows what
    /// the incoming data reports rather than only that a question exists.
    #[serde(default)]
    pub sd_holds: i64,
    #[serde(default)]
    pub sd_population_holds: i64,
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

    /// This source ships its own precomputed standard deviation and nothing says which divisor it
    /// used. The pairing is where that can first be asked, so it is asked here; leaving it unset
    /// is allowed and the audit gate is then the backstop.
    pub fn sd_estimator_undeclared(parameter: &str) -> Self {
        Self {
            kind: "sd_estimator_undeclared".to_string(),
            message: format!(
                "This source reports its own standard deviation for '{parameter}' and has not \
                 said which divisor it uses. Until one is declared, statistics use the sample \
                 formula (n-1) and disagreements matching the population divisor are held for a \
                 decision."
            ),
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
    curves: HashMap<Uuid, Vec<PlanCurveRef>>,
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

    let mut by_id = HashMap::new();
    let mut labels = Vec::new();
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
        });
    }

    let column = curve_column.clone()?;
    let stem = curve_column_stem(&column);

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
        });
    }

    Some(PlanInstrumentRef {
        curve_column: Some(column.clone()),
        id: None,
        name: format!("{stem} ({source_system} portal)"),
        source_key: format!("{source_system}:{column}"),
        resolved_by: "placeholder".to_string(),
        create: true,
        confirmed: false,
        stamps_readings,
        curves: vec![],
    })
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
        .query_all(Statement::from_sql_and_values(
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
        .query_all(Statement::from_sql_and_values(
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
            family_parameter_suggestion(&h.parameter, &catalog.params)
        } else {
            h.parameter.clone()
        };

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
            // Never guessed from the data: the source reported both conventions over the years,
            // so the review asks and the audit gate is the backstop if it is left unset.
            sd_estimator: None,
            sd_holds: sd_evidence.get(&stream.id).map_or(0, |e| e.0),
            sd_population_holds: sd_evidence.get(&stream.id).map_or(0, |e| e.1),
        };
        reclassify_entry(&mut entry, &catalog);
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
    pub readings_backfilled: u64,
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
pub async fn apply_plan(db: &sea_orm::DatabaseConnection, plan_id: Uuid) -> AppResult<ApplyResult> {
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

    refuse_unconfirmed_instruments(&entries)?;

    let txn = db.begin().await?;

    // Atomic status claim: a concurrent apply of the same plan matches zero rows and bails.
    // A rollback restores 'draft'.
    let claimed = txn
        .execute(Statement::from_sql_and_values(
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

    txn.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0".to_owned(),
    ))
    .await?;

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
    };

    let minted = mint_plan_instruments(&txn, &plan.source_system, &entries).await?;
    counters.instruments_created = minted.len() as u32;

    for entry in entries.iter().filter(|e| e.action == "pair") {
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

    let readings_backfilled = backfill_plan_readings(&txn, plan_id).await?;
    // Audit mismatches recorded while these streams were unpaired become reviewable with the
    // pairing they just gained.
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE replicate_audit_holds h SET status = 'pending'
         FROM data_streams ds
         WHERE ds.id = h.stream_id AND ds.pairing_plan_id = $1 AND h.status = 'deferred'",
        [plan_id.into()],
    ))
    .await?;
    finalize_plan(&txn, plan_id, &counters, readings_backfilled).await?;
    txn.commit().await?;

    // Re-derive the paired readings by the deployment + calibration windows for each touched
    // (site, parameter) slot, then a full refresh as a safety net. `backfill_plan_readings` only
    // stamps site_id/parameter_id; the window-aware engine (same one ingest/reprocess use) assigns
    // sensor_id/deployment_id/calibration_id and the per-window calibrated_value, while its recall
    // guard leaves pre-deployment history attributed by the pairing. Runs post-commit because the
    // reprocess opens its own transaction and refreshes continuous aggregates (which can't run
    // inside one).
    let slot_rows = db
        .query_all(Statement::from_sql_and_values(
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
        &entry.parameter.units,
        caches,
        &mut counters.sp_created,
        entry.sd_estimator.as_deref(),
    )
    .await?;
    Ok((site_parameter_id, parameter_id))
}

async fn resolve_or_create_site_param<C: ConnectionTrait>(
    txn: &C,
    site_id: Uuid,
    parameter_id: Uuid,
    units: &str,
    caches: &mut EntityCaches,
    sp_created: &mut u32,
    sd_estimator: Option<&str>,
) -> AppResult<Uuid> {
    // Refused rather than defaulted: the review chose this, and an unrecognised value is a bug in
    // the caller, not a licence to pick a divisor.
    let sd_estimator =
        crate::routes::private::readings::sd_estimator::parse_opt(sd_estimator)?;
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
        existing.id
    } else {
        let id = Uuid::new_v4();
        let mut param_name_val = caches.param_names.get(&parameter_id).cloned().unwrap_or_default();
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
            site_id: Set(site_id),
            parameter_id: Set(parameter_id),
            name: Set(param_name_val),
            sensor_type: Set(String::new()),
            sd_estimator: Set(sd_estimator.map(str::to_string)),
            display_units: Set(units_val.clone()),
            units_name: Set(units_val),
            units_min: Set(None),
            units_max: Set(None),
            decimal_places: Set(None),
            channel_id: Set(None),
            sample_interval_sec: Set(None),
            is_active: Set(Some(true)),
            is_public: Set(Some(false)),
            is_derived: Set(Some(false)),
            derived_definition_id: Set(None),
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

    let mut minted = HashMap::new();
    for (source_key, want) in wanted {
        if let Some(existing) = sensors::Entity::find()
            .filter(sensors::Column::SourceSystem.eq(source_system))
            .filter(sensors::Column::SourceKey.eq(source_key))
            .one(txn)
            .await?
        {
            minted.insert(source_key.to_string(), existing.id);
            continue;
        }
        let id = Uuid::new_v4();
        sensors::ActiveModel {
            id: Set(id),
            name: Set(Some(want.name.clone())),
            source_system: Set(Some(source_system.to_string())),
            source_key: Set(Some(source_key.to_string())),
            is_active: Set(Some(true)),
            is_lab_instrument: Set(Some(true)),
            data_frequency: Set("low".to_string()),
            created_at: Set(Some(Utc::now())),
            ..Default::default()
        }
        .insert(txn)
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
    // The plan's instrument, when the stream does not already name one. Deliberately no
    // deployment: a lab instrument corrects a grab, it is not stationed at the site, and the
    // "attributed but not deployed" state is the one `import_sensor_for_stream` documents.
    let from_plan = stream.sensor_id.is_none().then_some(instrument_id).flatten();

    if stream.sensor_id.is_none() && from_plan.is_none() {
        let site_id = site_parameters::Entity::find_by_id(site_parameter_id)
            .one(txn)
            .await?
            .map(|sp| sp.site_id)
            .unwrap_or_default();
        if let Err(e) = create_sensor_for_stream(txn, &stream, parameter_id, site_id).await {
            tracing::warn!(
                error = %e,
                stream_id = %stream.id,
                parameter_id = %parameter_id,
                site_id = %site_id,
                "Failed to auto-create sensor for stream during pairing; stream will still be paired",
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

async fn backfill_plan_readings<C: ConnectionTrait>(txn: &C, plan_id: Uuid) -> AppResult<u64> {
    let backfill_result = txn
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings r
          SET site_id = sp.site_id, parameter_id = sp.parameter_id,
              measurement_type = COALESCE(r.measurement_type, ds.measurement_type)
          FROM data_streams ds
          JOIN site_parameters sp ON ds.site_parameter_id = sp.id
          WHERE r.stream_id = ds.id AND r.site_id IS NULL
            AND ds.pairing_plan_id = $1",
            [plan_id.into()],
        ))
        .await?;

    // Replicate groups on the newly paired streams (2+ spot readings sharing a slot and timestamp,
    // e.g. migrated NOMIS A/B/C rows) form samples. The row-level triggers populate the statistics.
    crate::routes::private::readings::sample_groups::materialise_backfilled_samples(
        txn,
        "ds.pairing_plan_id = $1",
        plan_id.into(),
    )
    .await?;

    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE status_events se
          SET site_id = sp.site_id, parameter_id = sp.parameter_id
          FROM data_streams ds
          JOIN site_parameters sp ON ds.site_parameter_id = sp.id
          WHERE se.stream_id = ds.id AND se.site_id IS NULL
            AND ds.pairing_plan_id = $1",
        [plan_id.into()],
    ))
    .await?;

    Ok(backfill_result.rows_affected())
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
        .execute(Statement::from_sql_and_values(
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

    txn.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0".to_owned(),
    ))
    .await?;

    // NULL out readings for streams from this plan; samples formed by the pairing backfill
    // lose their last reference and are removed below
    // Samples referenced by this plan's readings, so only those can be removed below.
    let sample_ids: Vec<Uuid> = txn
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT DISTINCT r.sample_id AS id FROM readings r
              JOIN data_streams ds ON r.stream_id = ds.id
              WHERE ds.pairing_plan_id = $1 AND r.sample_id IS NOT NULL",
            [plan_id.into()],
        ))
        .await?
        .iter()
        .filter_map(|row| row.try_get::<Uuid>("", "id").ok())
        .collect();

    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE readings r SET site_id = NULL, parameter_id = NULL, sample_id = NULL
          FROM data_streams ds
          WHERE r.stream_id = ds.id AND ds.pairing_plan_id = $1",
        [plan_id.into()],
    ))
    .await?;

    // Reverting the pairing takes the reviewer away again; open reviews wait as deferred.
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE replicate_audit_holds h SET status = 'deferred'
         FROM data_streams ds
         WHERE ds.id = h.stream_id AND ds.pairing_plan_id = $1 AND h.status = 'pending'",
        [plan_id.into()],
    ))
    .await?;

    if !sample_ids.is_empty() {
        txn.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"DELETE FROM samples s
              WHERE s.id = ANY($1)
                AND NOT EXISTS (SELECT 1 FROM readings r WHERE r.sample_id = s.id)",
            [sample_ids.into()],
        ))
        .await?;
    }

    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE status_events se SET site_id = NULL, parameter_id = NULL
          FROM data_streams ds
          WHERE se.stream_id = ds.id AND ds.pairing_plan_id = $1",
        [plan_id.into()],
    ))
    .await?;

    // Unpair the streams; pairing_plan_id stays as the audit link back to this plan
    let result = txn
        .execute(Statement::from_sql_and_values(
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

    PlanSummary {
        total_streams: entries.len(),
        will_pair,
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
/// canonical and shared: `resolve_or_create_param` runs it as SQL at apply time and `bulk_pair`
/// builds the same precedence into its lookup map, so a review shows what apply will produce.
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
fn family_parameter_suggestion(name: &str, params: &[CatalogParam]) -> String {
    let stripped: String = name
        .split('_')
        .filter(|seg| !seg.eq_ignore_ascii_case("avg"))
        .collect::<Vec<_>>()
        .join("_");
    if stripped.is_empty() {
        return name.to_string();
    }
    if lookup_parameter_by_code_name_or_alias(&stripped, params).is_some() {
        return stripped;
    }
    if let Some((head, _)) = stripped.rsplit_once('_')
        && !head.is_empty()
        && lookup_parameter_by_code_name_or_alias(head, params).is_some()
    {
        return head.to_string();
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
        .query_all(Statement::from_string(
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
    // A family whose source reports an sd, on a slot that has not declared a divisor. `catalog`
    // has no slot rows, so this reads the plan's own declaration: an entry that has already been
    // patched with one is settled.
    if entry.sd_estimator.is_none()
        && entry
            .replicates
            .as_ref()
            .is_some_and(|r| r.portal_sd_column.is_some())
    {
        entry
            .warnings
            .push(PlanWarning::sd_estimator_undeclared(&entry.parameter.name));
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
        default_warning_min: Set(None),
        default_warning_max: Set(None),
        default_alarm_min: Set(None),
        default_alarm_max: Set(None),
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
