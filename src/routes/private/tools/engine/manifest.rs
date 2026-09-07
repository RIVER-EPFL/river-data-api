//! The manifest vocabulary: the typed declaration a tool script carries, its deserialisation and
//! the two switches over the param kinds, which belong beside each other.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{ResolvedParameter, ScriptInspection};

/// The closed `kind` vocabulary. `enum:` carries its variants after the colon.
const KINDS: [&str; 8] = [
    "number",
    "integer",
    "string",
    "boolean",
    "array",
    "object",
    "replicate_grid",
    "replicates",
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
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(untagged)]
pub enum ParamWhen {
    Note(String),
    Condition(ParamCondition),
}

// Hand-written because an untagged enum reports only that no variant matched, which hides a
// misspelled key of the condition object behind a message naming neither.
impl<'de> Deserialize<'de> for ParamWhen {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(de)?;
        if let serde_json::Value::String(note) = value {
            return Ok(Self::Note(note));
        }
        serde_json::from_value(value)
            .map(Self::Condition)
            .map_err(D::Error::custom)
    }
}

/// `{"param": "mode", "equals": "full_pipeline"}` or
/// `{"param": "mode", "any_of": ["p1", "p2"]}`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ParamCondition {
    pub param: String,
    #[serde(default)]
    pub equals: Option<serde_json::Value>,
    #[serde(default)]
    pub any_of: Option<Vec<serde_json::Value>>,
}

impl ParamCondition {
    pub(super) fn holds(&self, body: &serde_json::Map<String, serde_json::Value>) -> bool {
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    pub(super) fn check_value(&self, param: &str, value: &serde_json::Value) -> Result<(), String> {
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
    /// Help text shown beside the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Key of the manifest section the field renders under.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// `replicates` only: the catalog parameter (`parameters.code`) the entered replicates are
    /// readings of. The save stores each position as that parameter's reading at its index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameter_code: Option<String>,
    /// `replicates` only: how many rows the form opens with. The count is never a limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested: Option<u32>,
    /// `replicates` only: the curve slot whose chosen curve corrects the stored replicates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub curve: Option<String>,
    /// The catalog parameter `parameter_code` resolves to, filled by `GET /tools` against the
    /// database serving the request. Never authored, and never stored: a manifest travels between
    /// databases, so the resolution belongs to the response rather than to the declaration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameter: Option<ResolvedParameter>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    section: Option<String>,
    #[serde(default)]
    parameter_code: Option<String>,
    #[serde(default)]
    suggested: Option<u32>,
    #[serde(default)]
    curve: Option<String>,
}

// Hand-written so the checks run wherever a manifest is read, authoring included, rather than
// only where the engine happens to call a validator.
impl<'de> Deserialize<'de> for ManifestParam {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let raw = ManifestParamRaw::deserialize(de)?;
        check_kind(&raw.kind)
            .map_err(|e| D::Error::custom(format!("param '{}': {e}", raw.name)))?;
        if raw.kind == "replicates" {
            if raw.parameter_code.as_deref().is_none_or(str::is_empty) {
                return Err(D::Error::custom(format!(
                    "param '{}': replicates must name the parameter_code they are readings of",
                    raw.name
                )));
            }
            if raw.suggested == Some(0) {
                return Err(D::Error::custom(format!(
                    "param '{}': suggested must be at least 1",
                    raw.name
                )));
            }
        } else if raw.parameter_code.is_some() || raw.suggested.is_some() || raw.curve.is_some() {
            return Err(D::Error::custom(format!(
                "param '{}': parameter_code, suggested and curve belong to a replicates param",
                raw.name
            )));
        }
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
            description: raw.description,
            section: raw.section,
            parameter_code: raw.parameter_code,
            suggested: raw.suggested,
            curve: raw.curve,
            parameter: None,
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
#[serde(deny_unknown_fields)]
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
    /// Which divisor the samples saved from this output compute their standard deviation with:
    /// `sample` (n-1), `population` (n), or `selectable` to let the operator choose per run.
    /// Absent takes the slot's declaration, which is the usual case: the estimator is a property
    /// of the parameter, and only a tool that genuinely reports both conventions has cause to
    /// override it. Never reaches the R runner: it governs how the saved replicates are
    /// aggregated, not the calculation.
    #[serde(default)]
    pub sd_estimator: Option<String>,
    /// `mean` or `sd`: the engine computes this output over the curve-applied values of the
    /// `replicates` param `aggregate_of` names, so the preview a technician sees is the number
    /// the database will later serve, divisor included. The script never computes it; a script
    /// value under the same key is discarded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<String>,
}

impl ManifestOutput {
    /// The estimator this output fixes, or None when it defers to the slot (absent or
    /// `selectable`, which is the operator's choice rather than the manifest's).
    #[must_use]
    pub fn fixed_sd_estimator(&self) -> Option<&str> {
        match self.sd_estimator.as_deref() {
            Some(e @ ("sample" | "population")) => Some(e),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ManifestCurve {
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A titled group of fields on the entry form.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ManifestSection {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Sections are unique by key, and a param's `section` names one of them.
fn check_sections(raw: &ManifestRaw) -> Result<(), String> {
    for (i, s) in raw.sections.iter().enumerate() {
        if raw.sections[..i].iter().any(|other| other.key == s.key) {
            return Err(format!("section '{}' is declared twice", s.key));
        }
    }
    for p in &raw.params {
        if let Some(key) = p.section.as_deref()
            && !raw.sections.iter().any(|s| s.key == key)
        {
            return Err(format!(
                "param '{}': section '{key}' is not declared",
                p.name
            ));
        }
    }
    Ok(())
}

const fn default_true() -> bool {
    true
}

/// A site property the tool reads (elevation, latitude, ...), resolved from the `sites` row at
/// calculate time and recorded with its resolved value in the run. Fill-if-missing: a value the
/// request carries wins, so an operator can override the stored property exactly as the portal's
/// forms allow. A required property the site does not hold refuses the run naming it.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ManifestSiteInput {
    /// Column of the `sites` row, e.g. `altitude_m`.
    pub property: String,
    /// The manifest param the resolved value fills. Defaults to the property name.
    #[serde(default)]
    pub param: Option<String>,
    #[serde(default = "default_true")]
    pub required: bool,
}

impl ManifestSiteInput {
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
#[serde(deny_unknown_fields)]
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
    pub site_inputs: Vec<ManifestSiteInput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub event_inputs: Vec<ManifestEventInput>,
    /// Opaque QC block, stored as declared and served on `GET /tools` for clients that read it.
    /// Nothing server-side reads it: the seasonal check and the event audit take no input from
    /// the manifest. The only validation is that it is an object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qc: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<ManifestSection>,
    pub match_keywords: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestRaw {
    label: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    params: Vec<ManifestParam>,
    #[serde(default)]
    sections: Vec<ManifestSection>,
    #[serde(default)]
    outputs: Vec<ManifestOutput>,
    #[serde(default)]
    constants: Vec<String>,
    #[serde(default)]
    curves: Vec<ManifestCurve>,
    /// `station_inputs` is the spelling every stored manifest was written with; the entity is a
    /// site everywhere else in the schema, the API and the UI, so the key is `site_inputs` and the
    /// old one still parses.
    #[serde(default, alias = "station_inputs")]
    site_inputs: Vec<ManifestSiteInput>,
    #[serde(default)]
    event_inputs: Vec<ManifestEventInput>,
    #[serde(default)]
    qc: Option<serde_json::Value>,
    #[serde(default)]
    match_keywords: Vec<String>,
}

/// The vocabulary checks on one output. Read wherever a manifest is read, authoring included.
fn check_output(o: &ManifestOutput) -> Result<(), String> {
    if let Some(declared) = o.sd_estimator.as_deref()
        && !matches!(declared, "sample" | "population" | "selectable")
    {
        return Err(format!(
            "output '{}': sd_estimator '{declared}' is not 'sample', 'population' or 'selectable'",
            o.key
        ));
    }
    if let Some(agg) = o.aggregate.as_deref() {
        if !matches!(agg, "mean" | "sd") {
            return Err(format!(
                "output '{}': aggregate '{agg}' is not 'mean' or 'sd'",
                o.key
            ));
        }
        if o.aggregate_of.is_none() {
            return Err(format!(
                "output '{}': aggregate needs aggregate_of naming the replicates it reduces",
                o.key
            ));
        }
    }
    Ok(())
}

impl<'de> Deserialize<'de> for Manifest {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let raw = ManifestRaw::deserialize(de)?;
        check_sections(&raw).map_err(D::Error::custom)?;
        for p in &raw.params {
            if let Some(curve) = p.curve.as_deref()
                && !raw.curves.iter().any(|c| c.name == curve)
            {
                return Err(D::Error::custom(format!(
                    "param '{}': curve '{curve}' names no curve slot of the manifest",
                    p.name
                )));
            }
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
        // An engine-computed aggregate reduces a replicates param, so `aggregate_of` has to name
        // one; the plain display marker (aggregate_of without aggregate) stays free-form.
        for o in &raw.outputs {
            check_output(o).map_err(D::Error::custom)?;
            if o.aggregate.is_some()
                && let Some(source) = o.aggregate_of.as_deref()
                && !raw
                    .params
                    .iter()
                    .any(|p| p.name == source && p.kind == "replicates")
            {
                return Err(D::Error::custom(format!(
                    "output '{}': aggregate_of '{source}' names no replicates param of the \
                     manifest",
                    o.key
                )));
            }
        }
        // A resolved value reaches the runner as an input, so the field it fills has to be a
        // declared param: an undeclared target would be refused as an unknown field at call time.
        for s in &raw.site_inputs {
            let target = s.target();
            if !raw.params.iter().any(|p| p.name == target) {
                return Err(D::Error::custom(format!(
                    "site_input '{}': fills param '{target}', which the manifest does not declare",
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
            site_inputs: raw.site_inputs,
            event_inputs: raw.event_inputs,
            qc: raw.qc,
            sections: raw.sections,
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

/// Whether a value fits a manifest `kind`. Arrays and grids stay shallow: their element shapes
/// are the wrapper's contract, this only rejects the wrong container. An unknown kind cannot
/// reach here: `check_kind` rejects it when the manifest is read.
pub(super) fn kind_accepts(kind: &str, value: &serde_json::Value) -> bool {
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
        "replicates" => is_number_list(value),
        "object" => value.is_object(),
        _ => false,
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
