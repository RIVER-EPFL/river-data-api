//! Generic dispatch for DB-stored tool scripts.
//!
//! A tool is a row in `tool_scripts` whose active `tool_script_versions` row carries the R
//! script, its manifest (typed inputs, outputs, constants, curve slots) and its test cases.
//! Calculation resolves constants and curves here, so the runner receives values and the
//! provenance snapshot is taken where the data was read, then proxies to the OpenCPU runner.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

/// The serialization the API requires of the runner: full precision (the default rounds to 4
/// significant digits), scalars as scalars, and R NA as null.
const RUNNER_JSON_ARGS: &str = "auto_unbox=true&digits=17&na=null";

/// The closed `kind` vocabulary. `enum:` carries its variants after the colon.
const KINDS: [&str; 7] = [
    "number",
    "integer",
    "string",
    "boolean",
    "array",
    "object",
    "replicate_grid",
];

fn check_kind(kind: &str) -> Result<(), String> {
    if let Some(variants) = kind.strip_prefix("enum:") {
        if variants.is_empty() || variants.split('|').any(str::is_empty) {
            return Err(format!(
                "kind '{kind}' must list at least one non-empty variant (enum:a|b)"
            ));
        }
        return Ok(());
    }
    if KINDS.contains(&kind) {
        return Ok(());
    }
    Err(format!(
        "unknown kind '{kind}': expected one of {} or enum:<v1|v2>",
        KINDS.join(", ")
    ))
}

/// A param's `when`. A plain string is an advisory note and gates nothing; the object form is a
/// condition on an input's value and is what makes `required` conditional. The param it names is
/// only checked for membership, and one naming itself can never be enforced: requiredness is
/// consulted for an absent field, so the condition reads an absent value and does not hold.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum ParamWhen {
    Note(String),
    Condition(ParamCondition),
}

/// `{"param": "mode", "equals": "full_pipeline"}` or
/// `{"param": "mode", "any_of": ["p1", "p2"]}`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ParamCondition {
    pub param: String,
    #[serde(default)]
    pub equals: Option<serde_json::Value>,
    #[serde(default)]
    pub any_of: Option<Vec<serde_json::Value>>,
}

impl ParamCondition {
    fn holds(&self, body: &serde_json::Map<String, serde_json::Value>) -> bool {
        let Some(actual) = body.get(&self.param) else {
            return false;
        };
        if let Some(expected) = &self.equals {
            return actual == expected;
        }
        if let Some(accepted) = &self.any_of {
            return accepted.iter().any(|v| v == actual);
        }
        false
    }
}

/// How a structured param's value is laid out. `object` sends one object of fields, `rows` an
/// array of such objects (one per replicate), `lists` an object of number lists keyed by field
/// name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StructLayout {
    Object,
    Rows,
    Lists,
}

/// How the rows of a `rows` layout are labelled in the entry form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RowLabels {
    #[default]
    Letters,
    Numbers,
}

/// A field whose value is the difference of two other fields of the same row.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FieldFormula {
    /// `[minuend, subtrahend]`, both naming fields of the same structure.
    pub subtract: [String; 2],
}

/// One column of a structured param.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ManifestField {
    pub name: String,
    pub label: String,
    pub units: Option<String>,
    /// Whether a row that carries anything at all has to carry this field.
    pub required: bool,
    /// How many numbers the field holds. Above 1 the value is a list, entered as that many
    /// inputs; the count is what the form offers, not a length the request has to match.
    pub values: u32,
    /// Whether the field reaches the request body. A field that does not is typed on the bench
    /// to feed a computed field, or shown as a check on one.
    pub send: bool,
    pub computed: Option<FieldFormula>,
}

#[derive(Deserialize)]
struct ManifestFieldRaw {
    name: String,
    label: String,
    #[serde(default)]
    units: Option<String>,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    values: Option<u32>,
    #[serde(default)]
    send: Option<bool>,
    #[serde(default)]
    computed: Option<FieldFormula>,
}

/// What a structured param's value holds: the columns, and how they are arranged for entry.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ManifestStructure {
    pub layout: StructLayout,
    pub fields: Vec<ManifestField>,
    /// `rows` layout: rows offered before anything is entered.
    pub rows: u32,
    pub max_rows: Option<u32>,
    pub row_labels: RowLabels,
    /// `lists` layout: values per field.
    pub values: u32,
    pub value_labels: Vec<String>,
    /// Whether a field the structure does not declare may still be sent. True where the tool
    /// accepts more column spellings than the form offers.
    pub additional_fields: bool,
}

#[derive(Deserialize)]
struct ManifestStructureRaw {
    #[serde(default)]
    layout: Option<StructLayout>,
    fields: Vec<ManifestFieldRaw>,
    #[serde(default)]
    rows: Option<u32>,
    #[serde(default)]
    max_rows: Option<u32>,
    #[serde(default)]
    row_labels: Option<RowLabels>,
    #[serde(default)]
    values: Option<u32>,
    #[serde(default)]
    value_labels: Vec<String>,
    #[serde(default)]
    additional_fields: bool,
}

impl ManifestStructure {
    /// Read a declaration against the kind of the param carrying it, filling the defaults the
    /// layout implies. Everything a later reader relies on is settled here, so neither the form
    /// nor the request check has to re-derive it.
    fn resolve(kind: &str, raw: ManifestStructureRaw) -> Result<Self, String> {
        let implied = match kind {
            "replicate_grid" => StructLayout::Rows,
            "object" => StructLayout::Object,
            _ => {
                return Err(
                    "structure is declared only on an object or replicate_grid param".to_string(),
                );
            }
        };
        let layout = raw.layout.unwrap_or(implied);
        match (layout, kind) {
            (StructLayout::Rows, "replicate_grid")
            | (StructLayout::Object | StructLayout::Lists, "object") => {}
            _ => {
                return Err(format!(
                    "layout {} does not fit kind '{kind}': rows needs replicate_grid, object and lists need object",
                    serde_json::to_string(&layout).unwrap_or_default()
                ));
            }
        }
        if raw.fields.is_empty() {
            return Err("structure declares no fields".to_string());
        }

        let names: Vec<&str> = raw.fields.iter().map(|f| f.name.as_str()).collect();
        let mut fields = Vec::with_capacity(raw.fields.len());
        for f in &raw.fields {
            if f.name.trim().is_empty() || f.label.trim().is_empty() {
                return Err("every structure field needs a name and a label".to_string());
            }
            if names.iter().filter(|n| **n == f.name).count() > 1 {
                return Err(format!("field '{}' is declared more than once", f.name));
            }
            let values = f.values.unwrap_or(1);
            if values == 0 {
                return Err(format!("field '{}' must hold at least one value", f.name));
            }
            let send = f.send.unwrap_or(true);
            if f.required && !send {
                return Err(format!(
                    "field '{}' is not sent, so it cannot be required",
                    f.name
                ));
            }
            if let Some(formula) = &f.computed {
                if values > 1 {
                    return Err(format!(
                        "field '{}' is computed, so it holds one value",
                        f.name
                    ));
                }
                for operand in &formula.subtract {
                    if operand == &f.name || !names.contains(&operand.as_str()) {
                        return Err(format!(
                            "field '{}' is computed from '{operand}', which the structure does not declare",
                            f.name
                        ));
                    }
                }
            }
            if layout == StructLayout::Lists
                && (values > 1 || !send || f.computed.is_some() || f.required)
            {
                return Err(format!(
                    "field '{}': a lists layout holds one number list per field, with no computed, entry-only or required column",
                    f.name
                ));
            }
            fields.push(ManifestField {
                name: f.name.clone(),
                label: f.label.clone(),
                units: f.units.clone(),
                required: f.required,
                values,
                send,
                computed: f.computed.clone(),
            });
        }

        if layout != StructLayout::Rows
            && (raw.rows.is_some() || raw.max_rows.is_some() || raw.row_labels.is_some())
        {
            return Err("rows, max_rows and row_labels belong to a rows layout".to_string());
        }
        if layout != StructLayout::Lists && (raw.values.is_some() || !raw.value_labels.is_empty()) {
            return Err("values and value_labels belong to a lists layout".to_string());
        }
        let rows = match layout {
            StructLayout::Rows => raw.rows.unwrap_or(3),
            _ => 1,
        };
        if rows == 0 {
            return Err("rows must be at least 1".to_string());
        }
        if let Some(max) = raw.max_rows
            && max < rows
        {
            return Err(format!("max_rows {max} is below the {rows} rows offered"));
        }
        let values = match layout {
            StructLayout::Lists => raw.values.unwrap_or(1),
            _ => 1,
        };
        if values == 0 {
            return Err("values must be at least 1".to_string());
        }
        if !raw.value_labels.is_empty() && raw.value_labels.len() != values as usize {
            return Err(format!(
                "value_labels names {} of the {values} values",
                raw.value_labels.len()
            ));
        }

        Ok(Self {
            layout,
            fields,
            rows,
            max_rows: raw.max_rows,
            row_labels: raw.row_labels.unwrap_or_default(),
            values,
            value_labels: raw.value_labels,
            additional_fields: raw.additional_fields,
        })
    }

    fn field(&self, name: &str) -> Option<&ManifestField> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Whether a value fits the declaration. The checks are structural: which columns exist, and
    /// whether each holds a number or a list of them. Which of them a row has to carry for a
    /// result to come out is the wrapper's business, as it is in the portal.
    fn check_value(&self, param: &str, value: &serde_json::Value) -> Result<(), String> {
        match self.layout {
            StructLayout::Object => {
                let obj = value
                    .as_object()
                    .ok_or_else(|| format!("field '{param}' must be an object"))?;
                self.check_row(param, obj)
            }
            StructLayout::Rows => {
                let rows = value
                    .as_array()
                    .ok_or_else(|| format!("field '{param}' must be an array of rows"))?;
                if let Some(max) = self.max_rows
                    && rows.len() > max as usize
                {
                    return Err(format!("field '{param}' takes at most {max} rows"));
                }
                for (i, row) in rows.iter().enumerate() {
                    let obj = row.as_object().ok_or_else(|| {
                        format!("field '{param}' row {} must be an object", i + 1)
                    })?;
                    self.check_row(&format!("{param}[{}]", i + 1), obj)?;
                }
                Ok(())
            }
            StructLayout::Lists => {
                let obj = value
                    .as_object()
                    .ok_or_else(|| format!("field '{param}' must be an object of value lists"))?;
                for (key, list) in obj {
                    if self.field(key).is_none() && !self.additional_fields {
                        return Err(format!("field '{param}' declares no '{key}'"));
                    }
                    if list.is_null() {
                        continue;
                    }
                    if !is_number_list(list) {
                        return Err(format!("field '{param}.{key}' must be a list of numbers"));
                    }
                }
                Ok(())
            }
        }
    }

    fn check_row(
        &self,
        where_: &str,
        row: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), String> {
        for (key, value) in row {
            let Some(field) = self.field(key) else {
                if self.additional_fields {
                    continue;
                }
                return Err(format!("field '{where_}' declares no '{key}'"));
            };
            if !field.send {
                return Err(format!(
                    "field '{where_}.{key}' is entry-only and is not sent"
                ));
            }
            if value.is_null() {
                continue;
            }
            if field.values > 1 {
                if !is_number_list(value) {
                    return Err(format!("field '{where_}.{key}' must be a list of numbers"));
                }
            } else if !value.is_number() {
                return Err(format!("field '{where_}.{key}' must be a number"));
            }
        }
        Ok(())
    }
}

/// A list a blank cell may sit in: the entry form sends the row it has, gaps included.
fn is_number_list(value: &serde_json::Value) -> bool {
    value
        .as_array()
        .is_some_and(|items| items.iter().all(|v| v.is_number() || v.is_null()))
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ManifestParam {
    pub name: String,
    pub label: String,
    pub kind: String,
    pub units: Option<String>,
    pub required: bool,
    pub default: Option<serde_json::Value>,
    pub when: Option<ParamWhen>,
    /// What a structured param's value holds. Absent on a scalar param, and on a structured one
    /// whose columns nothing has declared yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structure: Option<ManifestStructure>,
}

#[derive(Deserialize)]
struct ManifestParamRaw {
    name: String,
    label: String,
    kind: String,
    #[serde(default)]
    units: Option<String>,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    default: Option<serde_json::Value>,
    #[serde(default)]
    when: Option<ParamWhen>,
    #[serde(default)]
    structure: Option<ManifestStructureRaw>,
}

// Hand-written so the checks run wherever a manifest is read, authoring included, rather than
// only where the engine happens to call a validator.
impl<'de> Deserialize<'de> for ManifestParam {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let raw = ManifestParamRaw::deserialize(de)?;
        check_kind(&raw.kind)
            .map_err(|e| D::Error::custom(format!("param '{}': {e}", raw.name)))?;
        if let Some(default) = &raw.default
            && !default.is_null()
            && !kind_accepts(&raw.kind, default)
        {
            return Err(D::Error::custom(format!(
                "param '{}': default {default} is not a {}",
                raw.name, raw.kind
            )));
        }
        if let Some(ParamWhen::Condition(c)) = &raw.when
            && c.equals.is_none()
            && c.any_of.is_none()
        {
            return Err(D::Error::custom(format!(
                "param '{}': when must carry 'equals' or 'any_of'",
                raw.name
            )));
        }
        let structure = match raw.structure {
            Some(declared) => Some(
                ManifestStructure::resolve(&raw.kind, declared)
                    .map_err(|e| D::Error::custom(format!("param '{}': {e}", raw.name)))?,
            ),
            None => None,
        };
        Ok(Self {
            name: raw.name,
            label: raw.label,
            kind: raw.kind,
            units: raw.units,
            required: raw.required,
            default: raw.default,
            when: raw.when,
            structure,
        })
    }
}

/// An output that may be saved names the catalog parameter it is saved to twice over.
///
/// `parameter_id` is authoritative when present, `suggested_parameter_code` is the fallback, and
/// resolution is id first then code. Both halves exist because a manifest has to survive leaving
/// the database it was authored in: the seeded tools are inserted into a fresh database where no
/// parameter UUID exists yet, and dev and production give the same analyte different UUIDs, so a
/// code-only output has to keep working exactly as it did.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ManifestOutput {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub units: Option<String>,
    #[serde(default)]
    pub per_replicate: bool,
    #[serde(default)]
    pub aggregate_of: Option<String>,
    #[serde(default)]
    pub parameter_id: Option<Uuid>,
    #[serde(default)]
    pub suggested_parameter_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ManifestCurve {
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub required: bool,
}

const fn default_true() -> bool {
    true
}

/// A station property the tool reads (elevation, latitude, ...), resolved from the `sites` row at
/// calculate time and recorded with its resolved value in the run. Fill-if-missing: a value the
/// request carries wins, so an operator can override the stored property exactly as the portal's
/// forms allow. A required property the site does not hold refuses the run naming it.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ManifestStationInput {
    /// Column of the `sites` row, e.g. `altitude_m`.
    pub property: String,
    /// The manifest param the resolved value fills. Defaults to the property name.
    #[serde(default)]
    pub param: Option<String>,
    #[serde(default = "default_true")]
    pub required: bool,
}

impl ManifestStationInput {
    #[must_use]
    pub fn target(&self) -> &str {
        self.param.as_deref().unwrap_or(&self.property)
    }
}

/// A same-event parameter read: when the request does not carry `param`, its value is resolved
/// from the collection event's stored readings (the served spot value: the sample mean, else the
/// lowest unflagged replicate). This is the portal's cross-tool prefill — pCO2 pulling field
/// temperature, DOM pulling the DOC average — as a declaration instead of R code.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ManifestEventInput {
    /// The manifest param this fills.
    pub param: String,
    /// Catalog parameter code (`parameters.code`) read at the event.
    pub parameter_code: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Manifest {
    pub label: String,
    pub description: Option<String>,
    pub params: Vec<ManifestParam>,
    pub outputs: Vec<ManifestOutput>,
    pub constants: Vec<String>,
    pub curves: Vec<ManifestCurve>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub station_inputs: Vec<ManifestStationInput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub event_inputs: Vec<ManifestEventInput>,
    /// QC declarations (replicate pooling, check exclusions), read by the seasonal check and the
    /// event audit. Stored as declared; the shape is an object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qc: Option<serde_json::Value>,
    pub match_keywords: Vec<String>,
}

#[derive(Deserialize)]
struct ManifestRaw {
    label: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    params: Vec<ManifestParam>,
    #[serde(default)]
    outputs: Vec<ManifestOutput>,
    #[serde(default)]
    constants: Vec<String>,
    #[serde(default)]
    curves: Vec<ManifestCurve>,
    #[serde(default)]
    station_inputs: Vec<ManifestStationInput>,
    #[serde(default)]
    event_inputs: Vec<ManifestEventInput>,
    #[serde(default)]
    qc: Option<serde_json::Value>,
    #[serde(default)]
    match_keywords: Vec<String>,
}

impl<'de> Deserialize<'de> for Manifest {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let raw = ManifestRaw::deserialize(de)?;
        for p in &raw.params {
            let Some(ParamWhen::Condition(c)) = &p.when else {
                continue;
            };
            if !raw.params.iter().any(|other| other.name == c.param) {
                return Err(D::Error::custom(format!(
                    "param '{}': when references unknown param '{}'",
                    p.name, c.param
                )));
            }
        }
        // A resolved value reaches the runner as an input, so the field it fills has to be a
        // declared param: an undeclared target would be refused as an unknown field at call time.
        for s in &raw.station_inputs {
            let target = s.target();
            if !raw.params.iter().any(|p| p.name == target) {
                return Err(D::Error::custom(format!(
                    "station_input '{}': fills param '{target}', which the manifest does not declare",
                    s.property
                )));
            }
        }
        for e in &raw.event_inputs {
            if !raw.params.iter().any(|p| p.name == e.param) {
                return Err(D::Error::custom(format!(
                    "event_input '{}': fills param '{}', which the manifest does not declare",
                    e.parameter_code, e.param
                )));
            }
        }
        if let Some(qc) = &raw.qc
            && !qc.is_object()
        {
            return Err(D::Error::custom("qc must be an object"));
        }
        Ok(Self {
            label: raw.label,
            description: raw.description,
            params: raw.params,
            outputs: raw.outputs,
            constants: raw.constants,
            curves: raw.curves,
            station_inputs: raw.station_inputs,
            event_inputs: raw.event_inputs,
            qc: raw.qc,
            match_keywords: raw.match_keywords,
        })
    }
}

/// Read a manifest from the JSON an author sent, naming the field that was refused.
///
/// `serde_json::from_value` reports the type error and discards the path, so a bad value anywhere
/// in a manifest reads as one sentence about a value the editor cannot point at.
pub fn parse_manifest(raw: &serde_json::Value) -> Result<Manifest, String> {
    serde_path_to_error::deserialize(raw).map_err(|e| {
        let path = e.path().to_string();
        let inner = e.into_inner();
        if path.is_empty() || path == "." {
            inner.to_string()
        } else {
            format!("{path}: {inner}")
        }
    })
}

/// Which half of an output's declaration the catalog row was found by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedBy {
    Id,
    Code,
}

/// The catalog parameter an output is saved to, resolved server-side. Serving this is what lets a
/// caller stop matching strings against a page of the catalog it happens to have fetched.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ResolvedParameter {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub default_units: Option<String>,
    /// True for a catalog entry created mechanically rather than by a person.
    pub needs_review: bool,
    pub resolved_by: ResolvedBy,
    /// True when the output declares a `parameter_id` no catalog row holds and the code resolved
    /// instead. Resolution still lands on a parameter, so nothing breaks, but the authoritative
    /// half points at a deleted row and wants repair.
    pub dangling_parameter_id: bool,
}

#[derive(Debug, Clone)]
struct CatalogRow {
    id: Uuid,
    code: String,
    name: String,
    default_units: Option<String>,
    needs_review: bool,
}

impl CatalogRow {
    fn resolved(&self, resolved_by: ResolvedBy, dangling_parameter_id: bool) -> ResolvedParameter {
        ResolvedParameter {
            id: self.id,
            code: self.code.clone(),
            name: self.name.clone(),
            default_units: self.default_units.clone(),
            needs_review: self.needs_review,
            resolved_by,
            dangling_parameter_id,
        }
    }
}

/// The catalog rows a set of manifests can possibly name, read in one query and indexed both ways,
/// so resolving every output of every tool costs one round trip rather than one per output.
#[derive(Debug, Default)]
pub struct ParameterCatalog {
    by_id: std::collections::HashMap<Uuid, CatalogRow>,
    /// Keyed by lowercased code, matching the `LOWER(code)` unique index.
    by_code: std::collections::HashMap<String, CatalogRow>,
}

impl ParameterCatalog {
    /// The parameter an output names: `parameter_id` when it resolves, else
    /// `suggested_parameter_code`, else nothing.
    #[must_use]
    pub fn resolve(&self, output: &ManifestOutput) -> Option<ResolvedParameter> {
        if let Some(id) = output.parameter_id
            && let Some(row) = self.by_id.get(&id)
        {
            return Some(row.resolved(ResolvedBy::Id, false));
        }
        let code = output.suggested_parameter_code.as_ref()?;
        self.by_code
            .get(&code.to_lowercase())
            .map(|row| row.resolved(ResolvedBy::Code, output.parameter_id.is_some()))
    }

    fn row_by_id(&self, id: Uuid) -> Option<&CatalogRow> {
        self.by_id.get(&id)
    }
}

/// Read every catalog row the given manifests could name, by id or by code.
pub async fn load_parameter_catalog<'a>(
    db: &DatabaseConnection,
    manifests: impl IntoIterator<Item = &'a Manifest>,
) -> AppResult<ParameterCatalog> {
    let mut ids: Vec<Uuid> = Vec::new();
    let mut codes: Vec<String> = Vec::new();
    for manifest in manifests {
        for output in &manifest.outputs {
            if let Some(id) = output.parameter_id {
                ids.push(id);
            }
            if let Some(code) = &output.suggested_parameter_code {
                codes.push(code.to_lowercase());
            }
        }
    }
    let mut catalog = ParameterCatalog::default();
    if ids.is_empty() && codes.is_empty() {
        return Ok(catalog);
    }
    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, code, name, default_units, needs_review FROM parameters
              WHERE id = ANY($1) OR LOWER(code) = ANY($2)",
            [
                sea_orm::Value::Array(
                    sea_orm::sea_query::ArrayType::Uuid,
                    Some(Box::new(ids.into_iter().map(Into::into).collect())),
                ),
                codes.into(),
            ],
        ))
        .await?;
    for row in &rows {
        let entry = CatalogRow {
            id: row.try_get("", "id")?,
            code: row.try_get("", "code")?,
            name: row.try_get("", "name")?,
            default_units: row.try_get("", "default_units")?,
            needs_review: row.try_get("", "needs_review")?,
        };
        catalog
            .by_code
            .insert(entry.code.to_lowercase(), entry.clone());
        catalog.by_id.insert(entry.id, entry);
    }
    Ok(catalog)
}

/// What a manifest's catalog references amount to when the version is saved.
///
/// An id that names no row is a refusal: it can only be a mistake, since nothing else could have
/// produced it. A code that names no row is reported instead, because an author may legitimately
/// declare an analyte before a manager creates it, and the seeded tools ship that way.
#[derive(Debug, Default)]
pub struct CatalogFindings {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Write the resolved code into the manifest JSON that is hashed, stored and served, so the
/// portable half of the declaration exists whatever the author sent.
fn stamp_code(raw: &mut serde_json::Value, index: usize, code: &str) {
    if let Some(output) = raw
        .get_mut("outputs")
        .and_then(|outputs| outputs.get_mut(index))
        .and_then(serde_json::Value::as_object_mut)
    {
        output.insert(
            "suggested_parameter_code".to_string(),
            serde_json::Value::String(code.to_string()),
        );
    }
}

/// Check a manifest's parameter and constant references against the catalog they will resolve
/// against at call time, and complete the parameter declarations that resolve.
///
/// An output that names an id and no code has the resolved code written into it, in `manifest` and
/// in `raw` when one is given: an id is meaningless in another database, so the half that travels
/// is stamped rather than left to the author to remember. An id and a code that name different
/// parameters are refused, since neither half can be preferred without guessing which analyte was
/// meant.
pub async fn check_manifest_against_catalog(
    db: &DatabaseConnection,
    manifest: &mut Manifest,
    mut raw: Option<&mut serde_json::Value>,
    missing_constant: MissingConstant,
) -> AppResult<CatalogFindings> {
    let mut findings = CatalogFindings::default();
    let catalog = load_parameter_catalog(db, std::iter::once(&*manifest)).await?;
    for (index, output) in manifest.outputs.iter_mut().enumerate() {
        if let Some(id) = output.parameter_id {
            match (catalog.row_by_id(id), &output.suggested_parameter_code) {
                (None, _) => findings.errors.push(format!(
                    "output '{}': parameter_id {id} is not in the parameter catalog",
                    output.key
                )),
                (Some(row), Some(code)) if !code.eq_ignore_ascii_case(&row.code) => {
                    findings.errors.push(format!(
                        "output '{}': parameter_id {id} is '{}' but suggested_parameter_code is \
                         '{code}'; an id and a code naming different parameters cannot both be \
                         what this output saves to",
                        output.key, row.code
                    ));
                }
                (Some(_), Some(_)) => {}
                (Some(row), None) => {
                    output.suggested_parameter_code = Some(row.code.clone());
                    if let Some(raw) = raw.as_deref_mut() {
                        stamp_code(raw, index, &row.code);
                    }
                }
            }
        }
        if let Some(code) = &output.suggested_parameter_code
            && catalog.resolve(output).is_none()
        {
            findings.warnings.push(format!(
                "output '{}': suggested_parameter_code '{code}' matches no parameter; \
                 saving this output needs a catalog entry",
                output.key
            ));
        }
    }
    // The slot an output saves to is the resolved parameter, so a collision is on the id and not
    // on the code: one output can name an id and another the code of that same row.
    let mut claimed: std::collections::HashMap<Uuid, String> = std::collections::HashMap::new();
    for output in &manifest.outputs {
        let Some(resolved) = catalog.resolve(output) else {
            continue;
        };
        if let Some(first) = claimed.insert(resolved.id, output.key.clone()) {
            findings.errors.push(format!(
                "outputs '{first}' and '{}' both resolve to parameter '{}' ({}); two outputs \
                 saving to one parameter write two series into one slot",
                output.key, resolved.code, resolved.id
            ));
        }
    }
    findings.errors.extend(
        missing_constants(db, &manifest.constants)
            .await?
            .into_iter()
            .map(|name| match missing_constant {
                MissingConstant::Refuse => {
                    format!("constant '{name}' is not in the constants table")
                }
                MissingConstant::Omit => format!(
                    "constant '{name}' is not in the constants table, so it did not reach the \
                     script; check the spelling or create the constant"
                ),
            }),
    );
    Ok(findings)
}

/// The declared constant names the `constants` table does not hold.
async fn missing_constants(db: &DatabaseConnection, names: &[String]) -> AppResult<Vec<String>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT name FROM constants WHERE name = ANY($1)",
            [names.to_vec().into()],
        ))
        .await?;
    let mut present = Vec::with_capacity(rows.len());
    for row in &rows {
        present.push(row.try_get::<String>("", "name")?);
    }
    Ok(names
        .iter()
        .filter(|name| !present.contains(name))
        .cloned()
        .collect())
}

/// One manifest output as `GET /tools` serves it: the declaration as authored, plus the parameter
/// it resolves to now. `parameter` is null when the output names none, or names one this database
/// does not hold.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ToolOutput {
    #[serde(flatten)]
    pub declared: ManifestOutput,
    pub parameter: Option<ResolvedParameter>,
}

/// One tool as `GET /tools` lists it: the manifest plus the identity of the version serving it.
#[derive(Debug, Serialize, ToSchema)]
pub struct ToolDescriptor {
    pub name: String,
    pub label: String,
    pub description: Option<String>,
    pub endpoint: String,
    pub params: Vec<ManifestParam>,
    pub outputs: Vec<ToolOutput>,
    pub constants: Vec<String>,
    pub curves: Vec<ManifestCurve>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub station_inputs: Vec<ManifestStationInput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub event_inputs: Vec<ManifestEventInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qc: Option<serde_json::Value>,
    pub match_keywords: Vec<String>,
    pub script_version_id: Uuid,
    pub version_no: i32,
}

/// The exact code identity a result was produced by, recorded into the provenance blob. The
/// runtime fields are null when the runner did not answer `runtime_info`: a number is still
/// worth serving without them. The version fields are null for a draft run, where the content
/// that produced the number is not stored anywhere.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ToolVersionRef {
    pub script_version_id: Option<Uuid>,
    pub version_no: Option<i32>,
    pub content_hash: String,
    pub runner_image: Option<String>,
    pub r_version: Option<String>,
}

/// What the runner reports about itself. It cannot change without the container restarting, so
/// it is fetched once and held until a runner failure invalidates it.
#[derive(Debug, Clone)]
pub struct RunnerRuntime {
    pub runner_image: Option<String>,
    pub r_version: Option<String>,
}

#[derive(Deserialize)]
struct RuntimeInfoResponse {
    #[serde(default)]
    r_version: Option<String>,
    #[serde(default)]
    image_build: Option<String>,
}

fn runtime_cell() -> &'static tokio::sync::RwLock<Option<RunnerRuntime>> {
    static CELL: std::sync::OnceLock<tokio::sync::RwLock<Option<RunnerRuntime>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| tokio::sync::RwLock::new(None))
}

pub async fn invalidate_runner_runtime() {
    *runtime_cell().write().await = None;
}

pub async fn runner_runtime(state: &AppState) -> Option<RunnerRuntime> {
    if let Some(cached) = runtime_cell().read().await.clone() {
        return Some(cached);
    }
    let fetched = fetch_runtime_info(state).await?;
    *runtime_cell().write().await = Some(fetched.clone());
    Some(fetched)
}

async fn fetch_runtime_info(state: &AppState) -> Option<RunnerRuntime> {
    let base = state.config.tools_runner_url.as_deref()?;
    let url = format!("{base}/library/riverdata.tools/R/runtime_info/json?auto_unbox=true");
    let response = runner_client()
        .post(&url)
        .timeout(std::time::Duration::from_secs(
            state.config.tools_runner_timeout_seconds,
        ))
        .json(&serde_json::json!({}))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let info: RuntimeInfoResponse = response.json().await.ok()?;
    Some(RunnerRuntime {
        runner_image: info.image_build,
        r_version: info.r_version,
    })
}

fn runner_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

pub struct ActiveTool {
    pub script_id: Uuid,
    pub name: String,
    pub label: String,
    pub description: Option<String>,
    pub version_id: Uuid,
    pub version_no: i32,
    pub script: String,
    pub entry_function: String,
    pub content_hash: String,
    pub manifest: Manifest,
}

fn stored_manifest(name: &str, raw: &serde_json::Value) -> AppResult<Manifest> {
    parse_manifest(raw)
        .map_err(|e| AppError::Internal(format!("tool '{name}' has an unreadable manifest: {e}")))
}

const ACTIVE_TOOL_SQL: &str = r"
    SELECT s.id AS script_id, s.name, s.label, s.description,
           v.id AS version_id, v.version_no, v.script, v.entry_function, v.manifest,
           v.content_hash
    FROM tool_scripts s
    JOIN tool_script_versions v ON v.id = s.active_version_id";

fn row_to_active(row: &sea_orm::QueryResult) -> AppResult<ActiveTool> {
    let name: String = row.try_get("", "name")?;
    let manifest = stored_manifest(&name, &row.try_get::<serde_json::Value>("", "manifest")?)?;
    Ok(ActiveTool {
        script_id: row.try_get("", "script_id")?,
        label: row.try_get("", "label")?,
        description: row.try_get("", "description")?,
        version_id: row.try_get("", "version_id")?,
        version_no: row.try_get("", "version_no")?,
        script: row.try_get("", "script")?,
        entry_function: row.try_get("", "entry_function")?,
        content_hash: row.try_get("", "content_hash")?,
        manifest,
        name,
    })
}

pub async fn list_active_tools(db: &DatabaseConnection) -> AppResult<Vec<ActiveTool>> {
    let rows = db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("{ACTIVE_TOOL_SQL} ORDER BY s.name"),
        ))
        .await?;
    rows.iter().map(row_to_active).collect()
}

pub async fn find_active_tool(db: &DatabaseConnection, name: &str) -> AppResult<ActiveTool> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!("{ACTIVE_TOOL_SQL} WHERE LOWER(s.name) = LOWER($1)"),
            [name.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Unknown tool: {name}")))?;
    row_to_active(&row)
}

impl ActiveTool {
    /// A tool assembled from editor content that is not stored. Its ids are nil because no row
    /// carries this content, and `version_ref` reports that as an absent version identity, so a
    /// draft result cannot be saved as if a version had produced it.
    pub fn draft(
        script: String,
        entry_function: String,
        manifest: Manifest,
        content_hash: String,
    ) -> Self {
        Self {
            script_id: Uuid::nil(),
            name: "draft".to_string(),
            label: manifest.label.clone(),
            description: manifest.description.clone(),
            version_id: Uuid::nil(),
            version_no: 0,
            script,
            entry_function,
            content_hash,
            manifest,
        }
    }

    pub fn descriptor(&self, catalog: &ParameterCatalog) -> ToolDescriptor {
        ToolDescriptor {
            name: self.name.clone(),
            label: self.manifest.label.clone(),
            description: self
                .manifest
                .description
                .clone()
                .or_else(|| self.description.clone()),
            endpoint: format!("/api/tools/{}/calculate", self.name),
            params: self.manifest.params.clone(),
            outputs: self
                .manifest
                .outputs
                .iter()
                .map(|output| ToolOutput {
                    parameter: catalog.resolve(output),
                    declared: output.clone(),
                })
                .collect(),
            constants: self.manifest.constants.clone(),
            curves: self.manifest.curves.clone(),
            station_inputs: self.manifest.station_inputs.clone(),
            event_inputs: self.manifest.event_inputs.clone(),
            qc: self.manifest.qc.clone(),
            match_keywords: self.manifest.match_keywords.clone(),
            script_version_id: self.version_id,
            version_no: self.version_no,
        }
    }

    pub fn version_ref(&self, runtime: Option<&RunnerRuntime>) -> ToolVersionRef {
        ToolVersionRef {
            script_version_id: (!self.version_id.is_nil()).then_some(self.version_id),
            version_no: (!self.version_id.is_nil()).then_some(self.version_no),
            content_hash: self.content_hash.clone(),
            runner_image: runtime.and_then(|r| r.runner_image.clone()),
            r_version: runtime.and_then(|r| r.r_version.clone()),
        }
    }
}

/// Whether a value fits a manifest `kind`. Arrays and grids stay shallow: their element shapes
/// are the wrapper's contract, this only rejects the wrong container. An unknown kind cannot
/// reach here: `check_kind` rejects it when the manifest is read.
fn kind_accepts(kind: &str, value: &serde_json::Value) -> bool {
    if let Some(variants) = kind.strip_prefix("enum:") {
        return value
            .as_str()
            .is_some_and(|s| variants.split('|').any(|v| v == s));
    }
    match kind {
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "array" | "replicate_grid" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

/// A curve as the runner receives it: coefficients plus, when it came from the catalog, the
/// identity that resolves them.
#[derive(Debug, Serialize)]
struct ResolvedCurve {
    slope: f64,
    intercept: f64,
    standard_curve_id: Option<Uuid>,
    label: Option<String>,
}

async fn resolve_curve(
    db: &DatabaseConnection,
    slot: &ManifestCurve,
    value: &serde_json::Value,
) -> AppResult<ResolvedCurve> {
    let obj = value.as_object().ok_or_else(|| {
        AppError::BadRequest(format!(
            "curve '{}' must be an object with slope/intercept or standard_curve_id",
            slot.name
        ))
    })?;

    if let Some(id) = obj.get("standard_curve_id").and_then(|v| v.as_str()) {
        let id: Uuid = id
            .parse()
            .map_err(|_| AppError::BadRequest(format!("curve '{}': invalid UUID", slot.name)))?;
        let row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT slope, intercept, name FROM standard_curves WHERE id = $1",
                [id.into()],
            ))
            .await?
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "curve '{}': standard curve {id} not found",
                    slot.name
                ))
            })?;
        return Ok(ResolvedCurve {
            slope: row.try_get("", "slope")?,
            intercept: row.try_get("", "intercept")?,
            standard_curve_id: Some(id),
            label: row.try_get("", "name").ok(),
        });
    }

    let coeff = |key: &str| {
        obj.get(key)
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(|| {
                AppError::BadRequest(format!("curve '{}': {key} must be a number", slot.name))
            })
    };
    Ok(ResolvedCurve {
        slope: coeff("slope")?,
        intercept: coeff("intercept")?,
        standard_curve_id: None,
        label: obj
            .get("label")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// What a declared constant the `constants` table does not hold means for the run that declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingConstant {
    /// A stored version cannot be saved declaring a name the table does not hold, so an absence at
    /// call time is the catalog having lost a row.
    Refuse,
    /// Editor content: the name is as likely half-typed as deleted, and refusing the run would
    /// withhold both the numbers and the finding the author is writing against.
    Omit,
}

async fn resolve_constants(
    db: &DatabaseConnection,
    names: &[String],
    missing: MissingConstant,
) -> AppResult<serde_json::Map<String, serde_json::Value>> {
    let mut out = serde_json::Map::new();
    if names.is_empty() {
        return Ok(out);
    }
    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT name, value FROM constants WHERE name = ANY($1)",
            [names.to_vec().into()],
        ))
        .await?;
    for row in &rows {
        let name: String = row.try_get("", "name")?;
        let value: f64 = row.try_get("", "value")?;
        out.insert(name, serde_json::json!(value));
    }
    if missing == MissingConstant::Refuse {
        // A version cannot be saved declaring a constant that does not exist, so reaching here
        // means the row was deleted after the fact: the state of the catalog, not the request.
        for name in names {
            if !out.contains_key(name) {
                return Err(AppError::Conflict(format!(
                    "constant '{name}' is declared by this tool but no longer exists in the \
                     constants table; restore it or publish a version that does not declare it"
                )));
            }
        }
    }
    Ok(out)
}

pub struct RunOutcome {
    pub results: serde_json::Map<String, serde_json::Value>,
    pub inputs_used: Vec<String>,
    pub inputs_ignored: Vec<String>,
    pub curves: Vec<serde_json::Value>,
    pub constants: serde_json::Map<String, serde_json::Value>,
    /// The inputs exactly as the runner received them: request values, plus defaults and the
    /// resolved station/event inputs, minus the curves. This is what the stored run records, so a
    /// recompute replays what actually ran rather than what the client happened to type.
    pub inputs: serde_json::Map<String, serde_json::Value>,
    /// Station properties resolved from the site, as `{property, param, value}`.
    pub station_inputs: Vec<serde_json::Value>,
    /// Same-event parameter reads, as `{param, parameter_code, parameter_id, value}`.
    pub event_inputs: Vec<serde_json::Value>,
    /// The calculation context the request declared, echoed for the stored run.
    pub site_id: Option<Uuid>,
    pub collected_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Pop the reserved context fields off a request body. They are calculation context, not tool
/// inputs: every tool accepts them and none receives them.
fn take_context(
    body: &mut serde_json::Map<String, serde_json::Value>,
) -> AppResult<(Option<Uuid>, Option<chrono::DateTime<chrono::Utc>>)> {
    let site_id = match body.remove("site_id") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .and_then(|s| s.parse::<Uuid>().ok())
                .ok_or_else(|| AppError::BadRequest("site_id must be a UUID".to_string()))?,
        ),
    };
    let collected_at = match body.remove("collected_at") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&chrono::Utc))
                .ok_or_else(|| {
                    AppError::BadRequest("collected_at must be an RFC 3339 timestamp".to_string())
                })?,
        ),
    };
    Ok((site_id, collected_at))
}

/// Fill the params the manifest's `station_inputs` declare from the `sites` row, where the request
/// did not carry them. Any column of the row is resolvable (D13); a required property the site
/// does not hold refuses the run naming it.
pub async fn resolve_station_inputs(
    db: &DatabaseConnection,
    tool_name: &str,
    manifest: &Manifest,
    site_id: Option<Uuid>,
    body: &mut serde_json::Map<String, serde_json::Value>,
) -> AppResult<Vec<serde_json::Value>> {
    let pending: Vec<&ManifestStationInput> = manifest
        .station_inputs
        .iter()
        .filter(|s| body.get(s.target()).is_none_or(serde_json::Value::is_null))
        .collect();
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    let Some(site_id) = site_id else {
        if let Some(required) = pending.iter().find(|s| s.required) {
            return Err(AppError::BadRequest(format!(
                "tool '{tool_name}' reads station property '{}'; pass site_id so it can be \
                 resolved, or supply '{}' directly",
                required.property,
                required.target()
            )));
        }
        return Ok(Vec::new());
    };
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT name, to_jsonb(s) AS site FROM sites s WHERE id = $1",
            [site_id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::BadRequest(format!("Site {site_id} not found")))?;
    let site_name: String = row.try_get("", "name")?;
    let site: serde_json::Value = row.try_get("", "site")?;

    let mut resolved = Vec::new();
    for s in pending {
        match site.get(&s.property) {
            Some(value) if !value.is_null() => {
                body.insert(s.target().to_string(), value.clone());
                resolved.push(serde_json::json!({
                    "property": s.property,
                    "param": s.target(),
                    "value": value,
                }));
            }
            _ if s.required => {
                return Err(AppError::BadRequest(format!(
                    "site '{site_name}' has no value for station property '{}', which tool \
                     '{tool_name}' requires",
                    s.property
                )));
            }
            _ => {}
        }
    }
    Ok(resolved)
}

/// Fill the params the manifest's `event_inputs` declare from the collection event's stored
/// readings, where the request did not carry them. The value is the served spot value: the sample
/// mean, else the lowest unflagged replicate. Absence is not an error here — the param's own
/// requiredness decides whether the run can proceed without it.
pub async fn resolve_event_inputs(
    db: &DatabaseConnection,
    manifest: &Manifest,
    site_id: Option<Uuid>,
    collected_at: Option<chrono::DateTime<chrono::Utc>>,
    body: &mut serde_json::Map<String, serde_json::Value>,
) -> AppResult<Vec<serde_json::Value>> {
    let pending: Vec<&ManifestEventInput> = manifest
        .event_inputs
        .iter()
        .filter(|e| body.get(&e.param).is_none_or(serde_json::Value::is_null))
        .collect();
    let (Some(site_id), Some(collected_at)) = (site_id, collected_at) else {
        return Ok(Vec::new());
    };
    let mut resolved = Vec::new();
    for e in pending {
        let Some(row) = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT p.id AS parameter_id, COALESCE(
                    (SELECT smp.mean FROM samples smp
                      WHERE smp.site_id = $1 AND smp.parameter_id = p.id
                        AND smp.collected_at = $3),
                    (SELECT COALESCE(r.calibrated_value, r.raw_value) FROM readings r
                      WHERE r.site_id = $1 AND r.parameter_id = p.id AND r.time = $3
                        AND r.measurement_type = 'spot' AND r.is_flagged IS NOT TRUE
                        AND r.withdrawn_at IS NULL
                      ORDER BY r.replicate_index LIMIT 1)
                 ) AS value
                 FROM parameters p WHERE LOWER(p.code) = LOWER($2)",
                [
                    site_id.into(),
                    e.parameter_code.clone().into(),
                    sea_orm::prelude::DateTimeWithTimeZone::from(collected_at).into(),
                ],
            ))
            .await?
        else {
            continue;
        };
        let parameter_id: Uuid = row.try_get("", "parameter_id")?;
        let Some(value) = row.try_get::<Option<f64>>("", "value")? else {
            continue;
        };
        body.insert(e.param.clone(), serde_json::json!(value));
        resolved.push(serde_json::json!({
            "param": e.param,
            "parameter_code": e.parameter_code,
            "parameter_id": parameter_id,
            "value": value,
        }));
    }
    Ok(resolved)
}

/// Validate a request body against the tool's manifest, resolve its constants and curves, and
/// execute the script in the runner.
pub async fn run_active_tool(
    state: &AppState,
    tool: &ActiveTool,
    body: &[u8],
) -> AppResult<RunOutcome> {
    run_tool_body(state, tool, body, None, MissingConstant::Refuse).await
}

/// The same path as [`run_active_tool`], with the option of taking constant values from the
/// caller instead of the catalog. A stored test case carries its own constants so it stays
/// reproducible whatever the `constants` table holds, and validation still has to exercise the
/// manifest handling the calculate path applies.
pub async fn run_tool_body(
    state: &AppState,
    tool: &ActiveTool,
    body: &[u8],
    constants_override: Option<&serde_json::Map<String, serde_json::Value>>,
    missing_constant: MissingConstant,
) -> AppResult<RunOutcome> {
    let body: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| AppError::BadRequest(format!("Invalid request body: {e}")))?;
    let mut body = match body {
        serde_json::Value::Object(map) => map,
        serde_json::Value::Null => serde_json::Map::new(),
        _ => {
            return Err(AppError::BadRequest(
                "request body must be a JSON object".to_string(),
            ));
        }
    };

    let (site_id, collected_at) = take_context(&mut body)?;

    let manifest = &tool.manifest;
    let param_names: Vec<&str> = manifest.params.iter().map(|p| p.name.as_str()).collect();
    let curve_names: Vec<&str> = manifest.curves.iter().map(|c| c.name.as_str()).collect();

    for key in body.keys() {
        if !param_names.contains(&key.as_str()) && !curve_names.contains(&key.as_str()) {
            return Err(AppError::BadRequest(format!(
                "unknown field '{key}' for tool '{}'",
                tool.name
            )));
        }
    }
    for p in &manifest.params {
        let Some(value) = body.get(&p.name).filter(|v| !v.is_null()) else {
            continue;
        };
        if !kind_accepts(&p.kind, value) {
            return Err(AppError::BadRequest(format!(
                "Invalid request body: field '{}' must be {} for tool '{}'",
                p.name, p.kind, tool.name
            )));
        }
        if let Some(structure) = &p.structure {
            structure.check_value(&p.name, value).map_err(|e| {
                AppError::BadRequest(format!(
                    "Invalid request body: {e} for tool '{}'",
                    tool.name
                ))
            })?;
        }
    }
    // Resolved context values land before defaults and requiredness: a typed value wins, a
    // resolved one fills the gap, and a manifest default is the last resort.
    let station_inputs =
        resolve_station_inputs(&state.db, &tool.name, manifest, site_id, &mut body).await?;
    let event_inputs =
        resolve_event_inputs(&state.db, manifest, site_id, collected_at, &mut body).await?;

    // Defaults land before requiredness so a condition reads the same values the runner will,
    // whatever order the params are declared in.
    for p in &manifest.params {
        let present = body.get(&p.name).is_some_and(|v| !v.is_null());
        if !present && let Some(default) = &p.default {
            body.insert(p.name.clone(), default.clone());
        }
    }
    for p in &manifest.params {
        if body.get(&p.name).is_some_and(|v| !v.is_null()) || !p.required {
            continue;
        }
        let enforced = match &p.when {
            None => true,
            Some(ParamWhen::Condition(c)) => c.holds(&body),
            Some(ParamWhen::Note(_)) => false,
        };
        if enforced {
            return Err(AppError::BadRequest(format!(
                "missing required field '{}' for tool '{}'",
                p.name, tool.name
            )));
        }
    }

    let mut curves = serde_json::Map::new();
    let mut curve_snapshots = Vec::new();
    let mut curves_consumed: Vec<String> = Vec::new();
    for slot in &manifest.curves {
        match body.remove(&slot.name) {
            Some(value) if !value.is_null() => {
                curves_consumed.push(slot.name.clone());
                let resolved = resolve_curve(&state.db, slot, &value).await?;
                let json = serde_json::to_value(&resolved).unwrap_or_default();
                curve_snapshots.push(serde_json::json!({ "name": slot.name, "curve": json }));
                curves.insert(slot.name.clone(), json);
            }
            _ if slot.required => {
                return Err(AppError::BadRequest(format!(
                    "missing required curve '{}' for tool '{}'",
                    slot.name, tool.name
                )));
            }
            _ => {}
        }
    }

    let constants = match constants_override {
        Some(supplied) => {
            let mut out = serde_json::Map::new();
            for name in &manifest.constants {
                let value = supplied.get(name).ok_or_else(|| {
                    AppError::BadRequest(format!(
                        "constant '{name}' is declared by the manifest but not supplied"
                    ))
                })?;
                out.insert(name.clone(), value.clone());
            }
            out
        }
        None => resolve_constants(&state.db, &manifest.constants, missing_constant).await?,
    };
    let provided: Vec<String> = body.keys().cloned().collect();
    let effective_inputs = body.clone();

    let raw = execute_script(
        state,
        &tool.script,
        &tool.entry_function,
        &serde_json::Value::Object(body),
        &serde_json::Value::Object(constants.clone()),
        &serde_json::Value::Object(curves),
    )
    .await?;

    let mut results = match raw {
        serde_json::Value::Object(map) => map,
        // An R empty named list serialises as []: every output was omitted.
        serde_json::Value::Array(a) if a.is_empty() => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("value".to_string(), other);
            map
        }
    };
    // NA outputs arrive as null; absent is the contract for an uncomputable value.
    results.retain(|_, v| !v.is_null());

    let declared_used: Vec<String> = results
        .remove("inputs_used")
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let mut inputs_used: Vec<String> = if declared_used.is_empty() {
        provided
            .iter()
            .filter(|k| param_names.contains(&k.as_str()))
            .cloned()
            .collect()
    } else {
        declared_used
            .into_iter()
            .filter(|k| provided.contains(k))
            .collect()
    };
    // A curve that was sent and resolved was consumed by definition.
    inputs_used.extend(curves_consumed);
    let inputs_ignored: Vec<String> = provided
        .iter()
        .filter(|k| !inputs_used.contains(k))
        .cloned()
        .collect();

    Ok(RunOutcome {
        results,
        inputs_used,
        inputs_ignored,
        curves: curve_snapshots,
        constants,
        inputs: effective_inputs,
        station_inputs,
        event_inputs,
        site_id,
        collected_at,
    })
}

/// Where a script failed to parse. `line`/`column` are absent when R's message carries no
/// position.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct ParseError {
    pub message: String,
    #[serde(default)]
    pub line: Option<i64>,
    #[serde(default)]
    pub column: Option<i64>,
}

/// A detection the parse tree cannot complete: `any` is what a caller branches on, the
/// expressions are what it shows when it does.
#[derive(Debug, Clone, Default, Deserialize, Serialize, ToSchema)]
pub struct DynamicFlag {
    pub any: bool,
    pub expressions: Vec<String>,
}

/// What the runner reads off a script's parse tree without evaluating it.
///
/// Every list is a floor rather than a complete set. A script that assembles names at runtime
/// (`out[[paste0(base, rep)]] <- ...`, the per-replicate outputs) cannot be read statically, and
/// that is what `dynamic_outputs` and `dynamic_reads` report: while either `any` is true, the
/// corresponding list is known to be short by an unknown amount.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct ScriptInspection {
    pub parse_ok: bool,
    /// Null when the script parses. A syntax error is a normal result, not a failed request.
    #[serde(default, deserialize_with = "deserialize_parse_error")]
    pub parse_error: Option<ParseError>,
    pub entry: String,
    pub entry_found: bool,
    /// The entry function's formals in declaration order; the runner calls them positionally.
    pub entry_args: Vec<String>,
    pub inputs: Vec<String>,
    pub constants: Vec<String>,
    pub curves: Vec<String>,
    /// The output keys read off the entry function. A floor: see `dynamic_outputs`.
    pub outputs: Vec<String>,
    pub dynamic_outputs: DynamicFlag,
    pub dynamic_reads: DynamicFlag,
    pub functions_defined: Vec<String>,
    pub functions_called: Vec<String>,
    /// The script's own top-level functions the entry function calls, which is what a tool
    /// depends on out of its prelude.
    pub script_functions_used: Vec<String>,
    pub libraries: Vec<String>,
    pub namespaces: Vec<String>,
}

/// R's empty list serialises as `[]`, which here means "no error" rather than a malformed one.
fn deserialize_parse_error<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<Option<ParseError>, D::Error> {
    let value = serde_json::Value::deserialize(de)?;
    if value.is_array() || value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

impl ScriptInspection {
    /// Whether the detected output list can be read as complete.
    #[must_use]
    pub fn outputs_complete(&self) -> bool {
        !self.dynamic_outputs.any
    }
}

/// What a script reads set against what a manifest declares. A pure comparison: it proposes no
/// manifest and changes neither side.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ManifestReconciliation {
    /// Read by the script, absent from `params`.
    pub undeclared_inputs: Vec<String>,
    pub undeclared_constants: Vec<String>,
    pub undeclared_curves: Vec<String>,
    /// Declared by the manifest, never read by the script.
    pub unread_params: Vec<String>,
    pub unread_constants: Vec<String>,
    pub unread_curves: Vec<String>,
    /// False when the script reads names built at runtime, which makes every `unread_*` entry
    /// possible rather than certain: the read may exist under a name the parse tree cannot show.
    pub reads_complete: bool,
    /// False when the script builds output keys at runtime, so the inspection's `outputs` is a
    /// floor and a manifest declaring more outputs than were detected is not thereby wrong.
    pub outputs_complete: bool,
}

fn missing_from(detected: &[String], declared: &[&str]) -> Vec<String> {
    detected
        .iter()
        .filter(|name| !declared.contains(&name.as_str()))
        .cloned()
        .collect()
}

fn unread(declared: &[&str], detected: &[String]) -> Vec<String> {
    declared
        .iter()
        .filter(|name| !detected.iter().any(|d| d == *name))
        .map(|name| (*name).to_string())
        .collect()
}

#[must_use]
pub fn reconcile_manifest(
    inspection: &ScriptInspection,
    manifest: &Manifest,
) -> ManifestReconciliation {
    let params: Vec<&str> = manifest.params.iter().map(|p| p.name.as_str()).collect();
    let constants: Vec<&str> = manifest.constants.iter().map(String::as_str).collect();
    let curves: Vec<&str> = manifest.curves.iter().map(|c| c.name.as_str()).collect();
    ManifestReconciliation {
        undeclared_inputs: missing_from(&inspection.inputs, &params),
        undeclared_constants: missing_from(&inspection.constants, &constants),
        undeclared_curves: missing_from(&inspection.curves, &curves),
        unread_params: unread(&params, &inspection.inputs),
        unread_constants: unread(&constants, &inspection.constants),
        unread_curves: unread(&curves, &inspection.curves),
        reads_complete: !inspection.dynamic_reads.any,
        outputs_complete: inspection.outputs_complete(),
    }
}

/// Read a script's parse tree in the runner. Nothing is evaluated, so a hostile or half-written
/// script is safe to inspect and a syntax error comes back as `parse_ok = false`.
pub async fn inspect_script(
    state: &AppState,
    script: &str,
    entry: &str,
) -> AppResult<ScriptInspection> {
    let raw = call_runner(
        state,
        "inspect_script",
        &serde_json::json!({ "script": script, "entry": entry }),
    )
    .await?;
    serde_json::from_value(raw).map_err(|e| {
        AppError::Internal(format!(
            "the tool runner returned an unreadable inspection: {e}"
        ))
    })
}

/// One call head the runner read off the parse tree, or one symbol read in value position.
/// A namespaced head arrives composed, as `pkg::fn`.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct ScannedName {
    pub name: String,
    pub line: i64,
}

/// One argument of one call. `name` is the argument's name where it had one, `kind` is
/// `string`, `symbol` or `other`, and `value` carries the literal or the symbol behind the first
/// two. A `library("curl")` and a `cat(f = "out.txt")` are both readable from this without
/// re-parsing the source.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct ScannedArg {
    pub call: String,
    pub name: String,
    pub value: String,
    pub kind: String,
    pub line: i64,
}

/// A script's call structure with line numbers, which is what the safety lint applies its policy
/// to. The runner reports structure only; which names are refused lives in `scripts.rs`.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct ScriptScan {
    pub parse_ok: bool,
    /// Null when the script parses. A syntax error is a normal result, not a failed request.
    #[serde(default, deserialize_with = "deserialize_parse_error")]
    pub parse_error: Option<ParseError>,
    #[serde(default)]
    pub calls: Vec<ScannedName>,
    /// Symbols read in value position that the script does not itself bind, which is where an
    /// alias (`runner <- system`) is visible.
    #[serde(default)]
    pub symbols: Vec<ScannedName>,
    #[serde(default)]
    pub args: Vec<ScannedArg>,
}

/// Read a script's call structure in the runner. Nothing is evaluated: `parse()` builds the tree
/// and the walk reads it, so scanning a hostile script is as safe as reading it.
pub async fn scan_script(state: &AppState, script: &str) -> AppResult<ScriptScan> {
    let raw = call_runner(
        state,
        "scan_script",
        &serde_json::json!({ "script": script }),
    )
    .await?;
    serde_json::from_value(raw).map_err(|e| {
        AppError::Internal(format!("the tool runner returned an unreadable scan: {e}"))
    })
}

/// The runner's syntax check on its own. `ok` with no message is a script that parses.
#[derive(Debug, Clone, Deserialize)]
pub struct ParseCheck {
    pub ok: bool,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub line: Option<i64>,
    #[serde(default)]
    pub column: Option<i64>,
}

pub async fn parse_check(state: &AppState, script: &str) -> AppResult<ParseCheck> {
    let raw = call_runner(
        state,
        "parse_check",
        &serde_json::json!({ "script": script }),
    )
    .await?;
    serde_json::from_value(raw).map_err(|e| {
        AppError::Internal(format!(
            "the tool runner returned an unreadable parse check: {e}"
        ))
    })
}

/// The runner's signal that the failure came from inside the tool: one JSON line on the first
/// line of a non-2xx body.
#[derive(Deserialize)]
struct RunnerToolError {
    error: String,
    message: String,
    #[serde(default)]
    call: Option<String>,
    #[serde(default)]
    traceback: Vec<String>,
}

fn parse_tool_error(body: &str) -> Option<RunnerToolError> {
    let first = body.lines().next()?;
    let parsed: RunnerToolError = serde_json::from_str(first).ok()?;
    (parsed.error == "tool_error").then_some(parsed)
}

/// POST a script to the runner. Connection failures are the runner being down (503); a non-2xx
/// is the R error text, which is the script author's diagnostic.
pub async fn execute_script(
    state: &AppState,
    script: &str,
    entry: &str,
    inputs: &serde_json::Value,
    constants: &serde_json::Value,
    curves: &serde_json::Value,
) -> AppResult<serde_json::Value> {
    call_runner(
        state,
        "run_tool",
        &serde_json::json!({
            "script": script,
            "entry": entry,
            "inputs": inputs,
            "constants": constants,
            "curves": curves,
        }),
    )
    .await
}

/// POST one `riverdata.tools` function. Every runner call goes through here so the URL, the
/// mandatory JSON arguments, the shared client, the timeout and the failure vocabulary are
/// decided once.
async fn call_runner(
    state: &AppState,
    function: &str,
    payload: &serde_json::Value,
) -> AppResult<serde_json::Value> {
    let Some(base) = state.config.tools_runner_url.as_deref() else {
        return Err(AppError::ServiceUnavailable(
            "the analytical tool runner is not configured (TOOLS_RUNNER_URL)".to_string(),
        ));
    };
    let url = format!("{base}/library/riverdata.tools/R/{function}/json?{RUNNER_JSON_ARGS}");

    let response = runner_client()
        .post(&url)
        .timeout(std::time::Duration::from_secs(
            state.config.tools_runner_timeout_seconds,
        ))
        .json(payload)
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(e) => {
            // The container may have restarted, so its reported runtime is no longer trusted.
            invalidate_runner_runtime().await;
            return Err(AppError::ServiceUnavailable(format!(
                "the analytical tool runner is unreachable: {e}"
            )));
        }
    };

    let status = response.status();
    let text = match response.text().await {
        Ok(text) => text,
        Err(e) => {
            invalidate_runner_runtime().await;
            return Err(AppError::ServiceUnavailable(format!(
                "the analytical tool runner failed mid-response: {e}"
            )));
        }
    };
    if !status.is_success() {
        if let Some(failure) = parse_tool_error(&text) {
            return Err(AppError::ToolScriptError {
                message: failure.message,
                call: failure.call,
                traceback: failure.traceback,
            });
        }
        // OpenCPU's own plain-text errors, raised before the tool is entered. The first lines
        // carry the R error message; the tail is the call echo.
        let message: String = text
            .lines()
            .take(4)
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();
        return Err(AppError::BadRequest(format!(
            "tool script error: {message}"
        )));
    }
    serde_json::from_str(&text)
        .map_err(|e| AppError::Internal(format!("the tool runner returned unparseable JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::{Manifest, ParamWhen, StructLayout, parse_tool_error};

    fn manifest_with(param: serde_json::Value) -> Result<Manifest, serde_json::Error> {
        serde_json::from_value(serde_json::json!({ "label": "T", "params": [param] }))
    }

    fn grid(structure: serde_json::Value) -> Result<Manifest, serde_json::Error> {
        manifest_with(serde_json::json!({
            "name": "replicates", "label": "Replicates", "kind": "replicate_grid",
            "structure": structure
        }))
    }

    #[test]
    fn a_declaration_fills_the_defaults_its_layout_implies() {
        let manifest = grid(serde_json::json!({
            "fields": [{ "name": "vol_ml", "label": "Vol", "units": "mL" }]
        }))
        .unwrap();
        let structure = manifest.params[0].structure.as_ref().unwrap();
        assert_eq!(structure.layout, StructLayout::Rows);
        assert_eq!(structure.rows, 3);
        assert_eq!(structure.fields[0].values, 1);
        assert!(structure.fields[0].send);
    }

    #[test]
    fn a_declaration_that_contradicts_its_param_is_refused() {
        for (param, expected) in [
            (
                serde_json::json!({ "name": "n", "label": "N", "kind": "number",
                    "structure": { "fields": [{ "name": "a", "label": "A" }] } }),
                "object or replicate_grid",
            ),
            (
                serde_json::json!({ "name": "o", "label": "O", "kind": "object",
                    "structure": { "layout": "rows", "fields": [{ "name": "a", "label": "A" }] } }),
                "does not fit kind",
            ),
        ] {
            let err = manifest_with(param).unwrap_err().to_string();
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn a_computed_field_must_name_fields_of_its_own_structure() {
        let err = grid(serde_json::json!({
            "fields": [
                { "name": "dried_g", "label": "Dried", "send": false },
                { "name": "afdm_g", "label": "AFDM",
                  "computed": { "subtract": ["dried_g", "ashed_g"] } }
            ]
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("ashed_g"), "{err}");
    }

    #[test]
    fn a_value_is_checked_against_the_columns_the_structure_declares() {
        let manifest = grid(serde_json::json!({
            "fields": [
                { "name": "vol_ml", "label": "Vol" },
                { "name": "diameters_cm", "label": "Diameters", "values": 3 },
                { "name": "dried_g", "label": "Dried", "send": false }
            ]
        }))
        .unwrap();
        let structure = manifest.params[0].structure.as_ref().unwrap();
        let check = |rows: serde_json::Value| structure.check_value("replicates", &rows);

        assert!(check(serde_json::json!([{ "vol_ml": 1.0, "diameters_cm": [1.0, null] }])).is_ok());
        // A blank row is what an untouched replicate looks like.
        assert!(check(serde_json::json!([{}])).is_ok());

        for (value, expected) in [
            (serde_json::json!([{ "nope": 1.0 }]), "declares no 'nope'"),
            (serde_json::json!([{ "dried_g": 1.0 }]), "entry-only"),
            (serde_json::json!([{ "vol_ml": [1.0] }]), "must be a number"),
            (
                serde_json::json!([{ "diameters_cm": 1.0 }]),
                "must be a list of numbers",
            ),
            (serde_json::json!([1.0]), "must be an object"),
        ] {
            let err = check(value).unwrap_err();
            assert!(err.contains(expected), "{err}");
            assert!(err.contains("replicates"), "{err}");
        }
    }

    #[test]
    fn an_open_structure_takes_a_column_it_does_not_declare() {
        let manifest = manifest_with(serde_json::json!({
            "name": "species", "label": "Species", "kind": "object",
            "structure": {
                "layout": "lists", "values": 3, "additional_fields": true,
                "fields": [{ "name": "NOx", "label": "NOx" }]
            }
        }))
        .unwrap();
        let structure = manifest.params[0].structure.as_ref().unwrap();
        assert!(
            structure
                .check_value("species", &serde_json::json!({ "TDN": [1.0, null, 3.0] }))
                .is_ok()
        );
        let err = structure
            .check_value("species", &serde_json::json!({ "TDN": 1.0 }))
            .unwrap_err();
        assert!(err.contains("list of numbers"), "{err}");
    }

    #[test]
    fn opencpu_plain_text_is_not_a_tool_error() {
        let body = "unused argument (nope = 1)\n\nIn call:\nrun_tool(nope = 1L)";
        assert!(parse_tool_error(body).is_none());
    }

    #[test]
    fn the_marker_line_carries_message_call_and_traceback() {
        let body = concat!(
            r#"{"error":"tool_error","message":"boom","call":"fn(x)","traceback":["fn(x)"]}"#,
            "\nBacktrace:\n  1. eval(call)"
        );
        let parsed = parse_tool_error(body).expect("first line parses");
        assert_eq!(parsed.message, "boom");
        assert_eq!(parsed.call.as_deref(), Some("fn(x)"));
        assert_eq!(parsed.traceback, vec!["fn(x)".to_string()]);
    }

    #[test]
    fn a_json_line_without_the_marker_falls_through() {
        let body = r#"{"error":"other","message":"boom"}"#;
        assert!(parse_tool_error(body).is_none());
    }

    #[test]
    fn a_manifest_kind_outside_the_vocabulary_is_refused() {
        let raw = serde_json::json!({
            "label": "T",
            "params": [{ "name": "hue", "label": "Hue", "kind": "colour" }]
        });
        let err = serde_json::from_value::<Manifest>(raw)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown kind 'colour'"), "{err}");
    }

    #[test]
    fn a_free_text_when_stays_a_note_and_an_object_becomes_a_condition() {
        let raw = serde_json::json!({
            "label": "T",
            "params": [
                { "name": "mode", "label": "Mode", "kind": "string" },
                { "name": "a", "label": "A", "kind": "number", "required": true,
                  "when": "mode=full" },
                { "name": "b", "label": "B", "kind": "number", "required": true,
                  "when": { "param": "mode", "equals": "full" } }
            ]
        });
        let manifest: Manifest = serde_json::from_value(raw).unwrap();
        assert!(matches!(manifest.params[1].when, Some(ParamWhen::Note(_))));
        let Some(ParamWhen::Condition(cond)) = &manifest.params[2].when else {
            panic!("the object form parses as a condition");
        };
        let mut body = serde_json::Map::new();
        assert!(!cond.holds(&body));
        body.insert("mode".into(), serde_json::json!("full"));
        assert!(cond.holds(&body));
        body.insert("mode".into(), serde_json::json!("simple"));
        assert!(!cond.holds(&body));
    }
}
