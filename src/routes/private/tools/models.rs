//! The tool catalog's shapes: the script entities, the manifest, and the request,
//! response and query rows the routes and the engine exchange.

use chrono::{DateTime, Utc};
use sea_orm::FromQueryResult;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::models::script::ToolScript;
use super::models::version::ToolScriptVersion;
use super::service::{ParameterCatalog, deserialize_parse_error};
use crate::routes::private::sync::models::HoldKind;
use crate::routes::private::sync::models::HoldStatus;

pub mod activation {
    use sea_orm::entity::prelude::*;

    /// One flip of a calculation's live version, under the caller who made it. Append-only: a
    /// rollback is another row, never an edit of the one it undoes.
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
    #[sea_orm(table_name = "tool_script_activations")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tool_script_id: Uuid,
        pub from_version_id: Option<Uuid>,
        pub to_version_id: Uuid,
        pub activated_by: Option<String>,
        pub activated_at: chrono::DateTime<chrono::Utc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::script::Entity",
            from = "Column::ToolScriptId",
            to = "super::script::Column::Id"
        )]
        Script,
    }

    impl Related<super::script::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Script.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod script {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    use crate::routes::private::tools::models::ToolScriptOperations;

    /// A calculation: its identity, which version is live, and whether it is in the calculation set.
    /// The code lives in `tool_script_versions`; nothing here is versioned.
    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
    )]
    #[sea_orm(table_name = "tool_scripts")]
    #[crudcrate(
        api_struct = "ToolScript",
        name_singular = "tool_script",
        name_plural = "tool_scripts",
        derive_partial_eq,
        operations = ToolScriptOperations
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        /// The stable machine name `/tools/{name}/calculate` is reached by. Lower-cased and refused
        /// unless `[a-z0-9_]` (`ToolScriptOperations`).
        #[sea_orm(unique)]
        #[crudcrate(filterable, fulltext, sortable, exclude(update))]
        pub name: String,
        #[crudcrate(fulltext, sortable)]
        pub label: String,
        #[crudcrate(fulltext)]
        pub description: Option<String>,
        /// The version `/tools` executes. Moved by activation, which also writes the audit row, so it
        /// is not editable here.
        #[crudcrate(exclude(create, update), filterable)]
        pub active_version_id: Option<Uuid>,
        #[crudcrate(exclude(create, update), sortable)]
        pub created_by: Option<String>,
        #[crudcrate(exclude(create, update), sortable)]
        pub created_at: chrono::DateTime<chrono::Utc>,
        #[crudcrate(exclude(create, update), sortable)]
        pub updated_at: chrono::DateTime<chrono::Utc>,
        /// Whether the tool is part of the calculation set: fired at visits by the chain, audited,
        /// and listed on the Tools page. Off, it can still be run by name.
        #[crudcrate(filterable, sortable, on_create = true)]
        pub enabled: bool,
        /// `script` (R in the sandbox) or `formula` (the definitions attached to the calculation).
        #[crudcrate(filterable, sortable, on_create = "script".to_string())]
        pub engine: String,
        /// The version history, newest first, without the code: a history is read to choose a
        /// version, and the content is fetched for the one that was chosen. Detail only, since a list
        /// of calculations is not a list of their versions.
        #[sea_orm(ignore)]
        #[crudcrate(non_db_attr = true, exclude(create, update, list), default = vec![])]
        pub versions: Vec<super::version::ToolScriptVersionList>,
        /// `version_no` of `active_version_id`, so a reader does not have to fetch the version to say
        /// which one is live.
        #[sea_orm(ignore)]
        #[crudcrate(non_db_attr = true, exclude(create, update), default = None)]
        pub active_version_no: Option<i32>,
        #[sea_orm(ignore)]
        #[crudcrate(non_db_attr = true, exclude(create, update), default = 0)]
        pub version_count: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(has_many = "super::version::Entity")]
        Versions,
    }

    impl Related<super::version::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Versions.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod version {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    /// One immutable version of a calculation: its code, the manifest it declares and the cases it
    /// must pass. Nothing updates a row here; a change is a new version, which is what makes a run's
    /// pinned `version_id` mean something.
    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
    )]
    #[sea_orm(table_name = "tool_script_versions")]
    #[crudcrate(
        api_struct = "ToolScriptVersion",
        name_singular = "tool_script_version",
        name_plural = "tool_script_versions",
        derive_partial_eq
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable, sortable)]
        pub tool_script_id: Uuid,
        #[crudcrate(filterable, sortable)]
        pub version_no: i32,
        /// The R source. Heavy, and a version list is a history rather than a reader.
        #[crudcrate(exclude(list))]
        pub script: String,
        pub entry_function: String,
        #[sea_orm(column_type = "JsonBinary")]
        #[crudcrate(exclude(list))]
        pub manifest: serde_json::Value,
        #[sea_orm(column_type = "JsonBinary")]
        #[crudcrate(exclude(list))]
        pub test_cases: serde_json::Value,
        #[crudcrate(filterable, sortable)]
        pub content_hash: String,
        #[crudcrate(sortable)]
        pub created_by: Option<String>,
        #[crudcrate(exclude(create, update), sortable)]
        pub created_at: chrono::DateTime<chrono::Utc>,
        /// When the stored cases last passed against the runner. A version goes live only on a pass
        /// taken at activation time, so this is a record, never a permission.
        #[crudcrate(sortable)]
        pub validated_at: Option<chrono::DateTime<chrono::Utc>>,
        /// What changed in this version and why, as its author wrote it.
        pub note: Option<String>,
        /// Whether the script points at this version. Filled per parent, not stored.
        #[sea_orm(ignore)]
        #[crudcrate(non_db_attr = true, exclude(create, update), default = false)]
        pub active: bool,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::script::Entity",
            from = "Column::ToolScriptId",
            to = "super::script::Column::Id"
        )]
        Script,
    }

    impl Related<super::script::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Script.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// A calculation's result as the runner returned it, stored nowhere.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct ToolCalculation {
    pub tool: String,
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub results: serde_json::Value,
    /// Outputs the script computed as NA. The portal blanked such a column rather than leaving
    /// the previous number standing, so these name the stored values a save must clear.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub cleared: Vec<String>,
    /// Formulas that did not run and why, as `{output, reason}`. An unresolved input costs its
    /// own output and no other, so the rest of the calculation is in `results`.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    #[schema(value_type = Vec<std::collections::HashMap<String, serde_json::Value>>)]
    pub skipped: Vec<serde_json::Value>,
    /// Outputs whose formula produced a number that is not finite. The calculation divided by
    /// zero, so the output is refused: it is in neither `results` nor `cleared`, the value already
    /// stored at the visit stands, and `skipped` carries the reason (Q172).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub refused: Vec<String>,
    pub inputs_used: Vec<String>,
    pub inputs_ignored: Vec<String>,
    /// The constant values the server resolved and passed to the runner, by name.
    /// The constant values the server resolved and passed to the runner, by name.
    #[schema(value_type = std::collections::HashMap<String, f64>)]
    pub constants: serde_json::Value,
    /// The curves the server resolved, as the runner received them.
    pub curves: Vec<CurveSnapshot>,
    /// Station properties resolved from the site named by `site_id`, as `{property, param, value}`.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    #[schema(value_type = Vec<std::collections::HashMap<String, serde_json::Value>>)]
    pub site_inputs: Vec<serde_json::Value>,
    /// Same-event parameter values resolved at `(site_id, collected_at)`, as
    /// `{param, parameter_code, parameter_id, value}`.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    #[schema(value_type = Vec<std::collections::HashMap<String, serde_json::Value>>)]
    pub event_inputs: Vec<serde_json::Value>,
    /// The exact script version and runtime that produced these numbers; goes into the
    /// provenance blob on save.
    pub tool_version: ToolVersionRef,
    /// Each formula as it was evaluated, with the values it read per cell. Absent for a script
    /// run, which returns only what the script returns.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub trace: Vec<TraceStep>,
    /// Every input as it was read, with the revision of each row behind it (Q215). Stored on
    /// the run's context, so a later reader can say whether a source has moved since.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub consumed: Vec<crate::routes::private::readings::models::ConsumedInput>,
}

/// A calculation and the `tool_runs` row it was stored as.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ToolResult {
    #[serde(flatten)]
    pub calculation: ToolCalculation,
    /// The stored `tool_runs` row for this calculation. A grab save names it as `tool_run_id`
    /// and the server builds the provenance blob from that row, never from the client.
    pub run_id: Uuid,
}

/// The closed `kind` vocabulary. `enum:` carries its variants after the colon.
pub(super) const KINDS: [&str; 8] = [
    "number",
    "integer",
    "string",
    "boolean",
    "array",
    "object",
    "replicate_grid",
    "replicates",
];

pub(super) fn check_kind(kind: &str) -> Result<(), String> {
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
    #[schema(required)]
    pub units: Option<String>,
    /// Whether a row that carries anything at all has to carry this field.
    pub required: bool,
    /// How many numbers the field holds. Above 1 the value is a list, entered as that many
    /// inputs; the count is what the form offers, not a length the request has to match.
    pub values: u32,
    /// Whether the field reaches the request body. A field that does not is typed on the bench
    /// to feed a computed field, or shown as a check on one.
    pub send: bool,
    #[schema(required)]
    pub computed: Option<FieldFormula>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ManifestFieldRaw {
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
    #[schema(required)]
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
pub(super) struct ManifestStructureRaw {
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
pub(super) fn is_number_list(value: &serde_json::Value) -> bool {
    value
        .as_array()
        .is_some_and(|items| items.iter().all(|v| v.is_number() || v.is_null()))
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ManifestParam {
    pub name: String,
    pub label: String,
    pub kind: String,
    #[schema(required)]
    pub units: Option<String>,
    pub required: bool,
    #[schema(required)]
    pub default: Option<serde_json::Value>,
    #[schema(required)]
    pub when: Option<ParamWhen>,
    /// What a structured param's value holds. Absent on a scalar param, and on a structured one
    /// whose columns nothing has declared yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub structure: Option<ManifestStructure>,
    /// Help text shown beside the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub description: Option<String>,
    /// Key of the manifest section the field renders under.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub section: Option<String>,
    /// `replicates` only: the catalog parameter (`parameters.code`) the entered replicates are
    /// readings of. The save stores each position as that parameter's reading at its index.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub parameter_code: Option<String>,
    /// `replicates` only: how many rows the form opens with. The count is never a limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub suggested: Option<u32>,
    /// `replicates` only: the curve slot whose chosen curve corrects the stored replicates.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub curve: Option<String>,
    /// The catalog parameter `parameter_code` resolves to, filled by `GET /tools` against the
    /// database serving the request. Never authored, and never stored: a manifest travels between
    /// databases, so the resolution belongs to the response rather than to the declaration.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub parameter: Option<ResolvedParameter>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ManifestParamRaw {
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
    /// `mean` or `sd`: the engine computes this output over the curve-applied values of the
    /// `replicates` param `aggregate_of` names, so the preview a technician sees is the number
    /// the database will later serve. The script never computes it; a script
    /// value under the same key is discarded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub aggregate: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ManifestCurve {
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub description: Option<String>,
}

/// A titled group of fields on the entry form.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ManifestSection {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub description: Option<String>,
}

/// Sections are unique by key, and a param's `section` names one of them.
pub(super) fn check_sections(raw: &ManifestRaw) -> Result<(), String> {
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

pub(super) const fn default_true() -> bool {
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

impl Manifest {
    /// Every catalog parameter code this calculation reads at a visit, lowercased: the event
    /// inputs, plus the parameter each `replicates` param names. A replicate family is a read
    /// edge like any other, even though it is never an event input (resolving one would put the
    /// group's single served value into a field that holds the repeats).
    #[must_use]
    pub fn read_codes(&self) -> Vec<String> {
        let mut codes: Vec<String> = self
            .event_inputs
            .iter()
            .map(|e| e.parameter_code.to_lowercase())
            .collect();
        for p in &self.params {
            if p.kind != "replicates" {
                continue;
            }
            if let Some(code) = &p.parameter_code {
                let code = code.to_lowercase();
                if !codes.contains(&code) {
                    codes.push(code);
                }
            }
        }
        codes
    }
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
    #[schema(required)]
    pub description: Option<String>,
    pub params: Vec<ManifestParam>,
    pub outputs: Vec<ManifestOutput>,
    pub constants: Vec<String>,
    pub curves: Vec<ManifestCurve>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub site_inputs: Vec<ManifestSiteInput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub event_inputs: Vec<ManifestEventInput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<ManifestSection>,
    pub match_keywords: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ManifestRaw {
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
    match_keywords: Vec<String>,
}

/// The vocabulary checks on one output. Read wherever a manifest is read, authoring included.
pub(super) fn check_output(o: &ManifestOutput) -> Result<(), String> {
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
        Ok(Self {
            label: raw.label,
            description: raw.description,
            params: raw.params,
            outputs: raw.outputs,
            constants: raw.constants,
            curves: raw.curves,
            site_inputs: raw.site_inputs,
            event_inputs: raw.event_inputs,
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

pub(super) fn missing_from(detected: &[String], declared: &[&str]) -> Vec<String> {
    detected
        .iter()
        .filter(|name| !declared.contains(&name.as_str()))
        .cloned()
        .collect()
}

pub(super) fn unread(declared: &[&str], detected: &[String]) -> Vec<String> {
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
    #[schema(required)]
    pub default_units: Option<String>,
    /// True for a catalog entry created mechanically rather than by a person.
    pub needs_review: bool,
    pub resolved_by: ResolvedBy,
    /// True when the output declares a `parameter_id` no catalog row holds and the code resolved
    /// instead. Resolution still lands on a parameter, so nothing breaks, but the authoritative
    /// half points at a deleted row and wants repair.
    pub dangling_parameter_id: bool,
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

/// One manifest output as `GET /tools` serves it: the declaration as authored, plus the parameter
/// it resolves to now. `parameter` is null when the output names none, or names one this database
/// does not hold.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ToolOutput {
    #[serde(flatten)]
    pub declared: ManifestOutput,
    #[schema(required)]
    pub parameter: Option<ResolvedParameter>,
}

/// One tool as `GET /tools` lists it: the manifest plus the identity of the version serving it.
#[derive(Debug, Serialize, ToSchema)]
pub struct ToolDescriptor {
    pub name: String,
    pub label: String,
    #[schema(required)]
    pub description: Option<String>,
    pub endpoint: String,
    pub params: Vec<ManifestParam>,
    pub outputs: Vec<ToolOutput>,
    pub constants: Vec<String>,
    pub curves: Vec<ManifestCurve>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub site_inputs: Vec<ManifestSiteInput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub event_inputs: Vec<ManifestEventInput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<ManifestSection>,
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
    #[schema(required)]
    pub script_version_id: Option<Uuid>,
    #[schema(required)]
    pub version_no: Option<i32>,
    pub content_hash: String,
    #[schema(required)]
    pub runner_image: Option<String>,
    #[schema(required)]
    pub r_version: Option<String>,
}

/// Which engine a calculation's versions carry (Q43). One concept, two ways of expressing the
/// arithmetic: R in the sandbox, or the formulas attached to the calculation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Script,
    Formula,
}

impl Engine {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "script" => Some(Self::Script),
            "formula" => Some(Self::Formula),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::Formula => "formula",
        }
    }
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
    pub engine: Engine,
    /// The formula engine's formulas, in no particular order; empty for a script calculation.
    pub formulas: Vec<PinnedFormula>,
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
            engine: Engine::Script,
            formulas: Vec::new(),
        }
    }

    /// A formula calculation assembled from an unsaved formula set. The script row is the stored
    /// one, so the run resolves against the calculation's own name; the version is nil because no
    /// row carries these formulas.
    pub fn draft_formulas(
        script: &ToolScript,
        manifest: Manifest,
        content_hash: String,
        formulas: Vec<PinnedFormula>,
    ) -> Self {
        Self {
            script_id: script.id,
            name: script.name.clone(),
            label: script.label.clone(),
            description: script.description.clone(),
            version_id: Uuid::nil(),
            version_no: 0,
            script: String::new(),
            entry_function: "formula".to_string(),
            content_hash,
            manifest,
            engine: Engine::Formula,
            formulas,
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
            // A replicates param names the catalog parameter its values are stored as, so the
            // response carries the resolution the same way an output's does.
            params: self
                .manifest
                .params
                .iter()
                .map(|p| ManifestParam {
                    parameter: p
                        .parameter_code
                        .as_deref()
                        .and_then(|code| catalog.resolve_code(code)),
                    ..p.clone()
                })
                .collect(),
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
            site_inputs: self.manifest.site_inputs.clone(),
            event_inputs: self.manifest.event_inputs.clone(),
            sections: self.manifest.sections.clone(),
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

/// A curve as the runner receives it: coefficients plus, when it came from the catalog, the
/// identity that resolves them.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ResolvedCurve {
    pub slope: f64,
    pub intercept: f64,
    #[schema(required)]
    pub standard_curve_id: Option<Uuid>,
    #[schema(required)]
    pub label: Option<String>,
}

/// One curve as the stored run records it: the slot it filled, and the curve itself.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CurveSnapshot {
    pub name: String,
    pub curve: ResolvedCurve,
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

pub struct RunOutcome {
    pub results: serde_json::Map<String, serde_json::Value>,
    /// Outputs the script computed as NA, which is a request to clear the stored value rather
    /// than an output it declined to name.
    pub cleared: Vec<String>,
    /// Formulas that did not run, as `{output, reason}`. An input a visit does not hold costs
    /// that one output and nothing else, so the run records which and why.
    pub skipped: Vec<serde_json::Value>,
    /// Outputs a formula refused because its result was not a finite number. Neither a value nor
    /// an NA: nothing is stored and nothing is withdrawn, and the chain raises a `skipped_output`
    /// finding naming the arithmetic (Q172). Each names its reason in `skipped` too.
    pub refused: Vec<String>,
    pub inputs_used: Vec<String>,
    pub inputs_ignored: Vec<String>,
    pub curves: Vec<CurveSnapshot>,
    pub constants: serde_json::Map<String, serde_json::Value>,
    /// The inputs exactly as the runner received them: request values, plus defaults and the
    /// resolved site/event inputs, minus the curves. This is what the stored run records, so a
    /// recompute replays what actually ran rather than what the client happened to type.
    pub inputs: serde_json::Map<String, serde_json::Value>,
    /// Station properties resolved from the site, as `{property, param, value}`.
    pub site_inputs: Vec<serde_json::Value>,
    /// Same-event parameter reads, as `{param, parameter_code, parameter_id, value}`.
    pub event_inputs: Vec<serde_json::Value>,
    /// The calculation context the request declared, echoed for the stored run.
    pub site_id: Option<Uuid>,
    pub collected_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Each formula as it was evaluated, in order. Empty for a script run.
    pub trace: Vec<TraceStep>,
    /// Every input as it was read, with the revision of each row behind it (Q215).
    pub consumed: Vec<crate::routes::private::readings::models::ConsumedInput>,
}

/// What the runner reports about itself. It cannot change without the container restarting, so
/// it is fetched once and held until a runner failure invalidates it.
#[derive(Debug, Clone)]
pub struct RunnerRuntime {
    pub runner_image: Option<String>,
    pub r_version: Option<String>,
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

impl ScriptInspection {
    /// Whether the detected output list can be read as complete.
    #[must_use]
    pub fn outputs_complete(&self) -> bool {
        !self.dynamic_outputs.any
    }
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

/// One formula of a calculation: what it computes, from what, and where the value goes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PinnedFormula {
    /// The definition's `code`; the output key on the run and in the provenance blob.
    pub code: String,
    pub label: String,
    pub units: Option<String>,
    pub formula: String,
    pub ordinal: i32,
    /// The catalog code of the parameter the value is stored under. `None` where the definition
    /// names no output parameter yet, which makes it unsavable but still evaluable.
    pub output_parameter_code: Option<String>,
    /// `(variable_name, parameter_code)`: the formula variable and the catalog parameter read
    /// into it.
    pub sources: Vec<(String, String)>,
    /// `(variable_name, site_property)`: the formula variable and the column of the site's own row
    /// read into it. A station's elevation is not a measurement anything took at a visit, so it is
    /// resolved from the site rather than asked for per event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub site_sources: Vec<(String, String)>,
    /// The curve slot this formula corrects with, if any. Inside the formula the slot's
    /// coefficients are the variables `curve_slope` and `curve_intercept`, so a calculation whose
    /// outputs take different curves declares a slot per formula rather than one per calculation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curve_slot: Option<String>,
    /// The variable whose replicate vector this formula evaluates over, one value per index.
    /// `None` is a formula producing one number. The output's replicate identity is the named
    /// input's, never one the calculation assigns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_replicate: Option<String>,
    /// A step of the calculation rather than a measurement: it stores nothing and is no output of
    /// the manifest, and its value reaches the formulas after it under this formula's own code.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub intermediate: bool,
}

/// The coefficients a resolved curve slot binds into a formula.
#[derive(Clone, Copy, Debug)]
pub struct Curve {
    pub slope: f64,
    pub intercept: f64,
}

/// What one formula of a run produced, in three states. `value` is `None` for a value computed as
/// not-a-number, which is the portal's NA and clears the stored value; `skipped` says the formula
/// never ran, which names no output at all; `refused` says it ran and produced a number that is
/// not finite, which stores nothing and leaves the value already at the visit standing (Q172).
#[derive(Clone, Debug, PartialEq)]
pub struct Evaluated {
    pub code: String,
    pub value: Option<f64>,
    pub curve_slot: Option<String>,
    pub skipped: Option<String>,
    pub refused: bool,
    /// The variables the formula read, by name, in the order the formula names them. Empty when
    /// the formula was skipped.
    pub bindings: Vec<(String, f64)>,
    /// The families the formula reduced, in the order it names them. Empty for a formula that
    /// reduces nothing.
    pub reductions: Vec<TraceReduction>,
}

/// A stored run replayed under the version it pinned, so a value computed months ago still shows
/// the formula behind it and the numbers that formula read.
#[derive(Debug, Serialize, ToSchema)]
pub struct RunTrace {
    pub run_id: Uuid,
    /// The calculation's name, as the run recorded it.
    pub tool: String,
    /// The pinned version's label, which is what a reader recognises the calculation by.
    pub label: String,
    pub version_no: i32,
    /// The visit the run read its values at, when it named one. A binding that is neither a step
    /// nor a constant was read here, which is the answer to "where did this number come from".
    #[schema(required)]
    pub site_id: Option<Uuid>,
    #[schema(required)]
    pub collected_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The visit's stored values the run read, as `{param, parameter_code, parameter_id, value}`.
    pub event_inputs: Vec<serde_json::Value>,
    /// The station properties the run read, as `{property, param, value}`.
    pub site_inputs: Vec<serde_json::Value>,
    /// The constant values the run resolved, by name.
    #[schema(value_type = std::collections::HashMap<String, f64>)]
    pub constants: serde_json::Value,
    pub trace: Vec<TraceStep>,
    /// The values the run produced, as it stored them: an explicit null is a value computed and
    /// not a number, as against never computed at all.
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub results: serde_json::Value,
    /// Outputs the run did not produce, each with its reason, as the run recorded them.
    pub skipped: Vec<serde_json::Value>,
    /// The curves the run applied, as it stored them.
    #[schema(value_type = Vec<serde_json::Value>)]
    pub curves: serde_json::Value,
    /// The manifest of the version the run pinned, so the stored result reads through the same
    /// tables a fresh run does rather than through a second rendering of its own.
    #[schema(value_type = Object)]
    pub manifest: serde_json::Value,
}

/// One formula of a run as it was evaluated: the text, and per cell the value it produced and
/// the variables it read. A scalar formula has one cell; a per-replicate one has one per index.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub struct TraceStep {
    pub code: String,
    pub label: String,
    #[schema(required)]
    pub units: Option<String>,
    /// The catalog parameter this formula writes, when it writes one. A reader arrives at a trace
    /// holding a parameter, so this is what says which step produced the value in front of them.
    #[schema(required)]
    pub output_parameter_code: Option<String>,
    pub formula: String,
    pub intermediate: bool,
    pub per_replicate: bool,
    pub cells: Vec<TraceCell>,
}

/// What one evaluation of a formula read and produced. `bindings` is keyed by the variable name
/// as the formula spells it; a value bound as not-a-number serialises as null.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub struct TraceCell {
    /// The replicate index, absent for a scalar formula.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub index: Option<usize>,
    #[schema(required)]
    pub value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub skipped: Option<String>,
    #[schema(value_type = std::collections::HashMap<String, f64>)]
    pub bindings: std::collections::BTreeMap<String, f64>,
    /// One entry per reducer the formula applied. A binding names one number; this names the
    /// family behind it, so a reader can see which repeats the mean or the sd was taken over.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reductions: Vec<TraceReduction>,
}

/// One reduction over a replicate family: the call as the formula writes it, the number it bound,
/// and the indexes whose values were eligible. A gap and a value computed as NA are not members,
/// so `members` is what the statistic was actually taken over.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub struct TraceReduction {
    pub call: String,
    #[schema(required)]
    pub value: Option<f64>,
    pub members: Vec<usize>,
}

/// What a calculation produced for one output: one number, or one per replicate index.
///
/// A per-replicate output keeps its gaps: a repeat that was not measured is a `None` at that
/// index, never a shorter list, because the index is the source's column position and closing up
/// would re-label every value after it.
#[derive(Clone, Debug, PartialEq)]
pub enum Produced {
    Scalar(Evaluated),
    PerReplicate {
        code: String,
        values: Vec<Option<f64>>,
        curve_slot: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, ToSchema, FromQueryResult)]
pub struct ImpactParameter {
    pub parameter_id: Uuid,
    pub parameter_code: String,
}

/// One calculation a set of parameters feeds.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CalculationImpact {
    pub tool: String,
    pub label: String,
    /// The parameters from the set this calculation reads, directly or through another
    /// calculation's output.
    pub reads: Vec<ImpactParameter>,
    /// The output parameters this calculation rewrites at the visit.
    pub outputs: Vec<ImpactParameter>,
}

/// What a change is being asked about. Every subject resolves to the parameters it moves, and the
/// answer is then the same walk over the same graph, so "what depends on this" has one meaning
/// whichever end it is asked from.
#[derive(Debug, Clone)]
pub enum Subject {
    /// Global parameters, the form every other subject reduces to.
    Parameters(Vec<Uuid>),
    /// A calibration: the parameters whose readings it corrects.
    Calibration(Uuid),
    /// One site parameter row.
    Slot(Uuid),
    /// One reading, by the key the curation routes use.
    Reading {
        stream_id: Uuid,
        replicate_index: Option<i32>,
    },
    /// A calculation, by name: what its own outputs feed downstream.
    Calculation(String),
    /// One constant, by id: the parameters the calculations declaring it publish.
    Constant(Uuid),
}

/// A version's content after Postgres has had its say about the JSON halves: the bytes to store
/// and the hash of exactly those bytes.
pub struct StoredVersionContent {
    pub content_hash: String,
    /// `manifest` and `test_cases` as `jsonb` renders them, to be written back with a `::jsonb`
    /// cast. Postgres' rendering re-parses to the same `jsonb`, so storing it changes nothing
    /// about the value and pins what the hash was taken over.
    pub manifest: String,
    pub test_cases: String,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct ClosureQuery {
    /// Global parameter ids, comma-separated: the values whose consequences are being asked about.
    pub parameter_ids: Option<String>,
    /// A calibration whose consequences are being asked about, instead of a parameter list.
    pub calibration_id: Option<Uuid>,
    /// A site parameter row, instead of a parameter list.
    pub site_parameter_id: Option<Uuid>,
    /// One reading's stream, instead of a parameter list.
    pub stream_id: Option<Uuid>,
    /// A calculation by name: what its own outputs feed downstream.
    pub calculation: Option<String>,
    /// A constant whose consequences are being asked about: where is this value used.
    pub constant_id: Option<Uuid>,
    /// Confine the coverage counts to one site. Every site when omitted.
    pub site_id: Option<Uuid>,
    /// Include the per-slot coverage of every calculation input and output. Off by default: it is
    /// three aggregate queries and a closure asked for before a write does not need it.
    #[serde(default)]
    pub include_coverage: bool,
}

/// What the store holds for one parameter of a calculation: whether anyone configured the slot,
/// how much is there, and where it came from.
#[derive(Debug, Clone, Serialize, ToSchema, FromQueryResult)]
pub struct SlotCoverage {
    pub parameter_id: Uuid,
    pub parameter_code: String,
    /// Sites with a `site_parameters` row for this parameter. Zero is the state an admin needs to
    /// see: a calculation input nobody has configured anywhere.
    pub sites_configured: i64,
    pub reading_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub first_reading: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub last_reading: Option<DateTime<Utc>>,
    /// The source systems the stored values arrived on, so a value that came from the portal is
    /// not mistaken for one a run produced.
    pub source_systems: Vec<String>,
    /// The minting paths of the tool runs behind the stored values: `interactive`, `csv_import` or
    /// `chain`. Empty means nothing here was produced by a run.
    pub run_sources: Vec<String>,
}

/// How one calculation is standing: the open event-audit findings against its outputs, and the
/// visits they sit on, which is the set an "apply to the stale visits" run would cover.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CalculationHealth {
    /// The calculation's name, as the findings record it.
    pub tool: String,
    /// Visits carrying at least one open finding this calculation raised.
    pub stale_visits: i64,
    pub missing_outputs: i64,
    pub stale_outputs: i64,
    pub skipped_outputs: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ClosureResponse {
    /// The calculations the named parameters feed, in the order the chain would run them.
    pub calculations: Vec<CalculationImpact>,
    /// Coverage per calculation input and output, when `include_coverage` asked for it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub coverage: Vec<SlotCoverage>,
    /// What the subject has already been used to compute, from the stored provenance rather than
    /// from the manifests. Present for a constant, which is the subject asked about before an edit
    /// that would move every one of these values.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub stored: Option<StoredUsage>,
}

/// How much of the record a subject is written into: the visits and the readings whose stored
/// provenance names it.
#[derive(Debug, Serialize, ToSchema)]
pub struct StoredUsage {
    /// The name the provenance records the subject under.
    pub name: String,
    pub visits: i64,
    pub readings: i64,
}

/// What one version of a calculation has already produced: the readings whose stored provenance
/// names it, and the visits those readings belong to. A version that produced nothing carries
/// zeros, because "nothing stored" and "not counted" are different claims.
#[derive(Debug, Serialize, ToSchema)]
pub struct VersionUsage {
    pub version_id: Uuid,
    pub version_no: i32,
    pub visits: i64,
    pub readings: i64,
}

pub struct ToolScriptOperations;

#[derive(Debug, Serialize, ToSchema)]
pub struct LintFinding {
    /// The script line the finding sits on. Zero for a finding about the manifest, which has no
    /// line in the script the editor shows.
    pub line: usize,
    pub message: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateScriptRequest {
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    /// `script` (the default) or `formula`. A formula calculation has no authored versions: its
    /// versions are minted from the definitions attached to it.
    #[serde(default)]
    pub engine: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateScriptRequest {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Switch the tool in or out of the calculation set. A disabled tool keeps its versions and
    /// activation and fires at no visit until it is switched back on.
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateVersionRequest {
    pub script: String,
    #[serde(default)]
    pub entry_function: Option<String>,
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub manifest: serde_json::Value,
    #[serde(default)]
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub test_cases: Option<serde_json::Value>,
    /// Short free text: what changed in this version and why.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CreateVersionResponse {
    pub version: ToolScriptVersion,
    /// What was stored anyway but is worth saying: an output whose `suggested_parameter_code`
    /// matches no catalog parameter. Empty on a version with nothing to report.
    pub lint: Vec<LintFinding>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct DraftRunRequest {
    pub script: String,
    #[serde(default)]
    pub entry_function: Option<String>,
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub manifest: serde_json::Value,
    /// The calculate request body this draft is run with: the manifest's params, plus its curve
    /// slots given either as a `standard_curve_id` or as literal coefficients.
    #[serde(default)]
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub inputs: Option<serde_json::Value>,
    /// Constant values in place of the catalog. As in a stored test case, an override must name
    /// every constant the manifest declares; omit the field to read the catalog.
    #[serde(default)]
    #[schema(value_type = Option<std::collections::HashMap<String, f64>>)]
    pub constants: Option<serde_json::Map<String, serde_json::Value>>,
}

/// What the script produced, present only when the run reached the end.
#[derive(Debug, Serialize, ToSchema)]
pub struct DraftRunResults {
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub results: serde_json::Value,
    pub inputs_used: Vec<String>,
    pub inputs_ignored: Vec<String>,
    /// The constant values the server resolved and passed to the runner, by name.
    #[schema(value_type = std::collections::HashMap<String, f64>)]
    pub constants: serde_json::Value,
    pub curves: Vec<CurveSnapshot>,
}

/// Which of the three things that end a draft run happened, so a caller can render the failure
/// where it belongs (the body form, the script pane, or the runner's own state) instead of
/// reading the message.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DraftRunFailureKind {
    /// The manifest refused the request body: an undeclared field, a wrong kind, a missing
    /// requirement, or a curve that does not resolve.
    BodyRefused,
    /// The script raised. `call` and `traceback` carry what R reported.
    ScriptError,
    /// The runner is unconfigured or did not answer.
    RunnerUnavailable,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DraftRunFailure {
    pub kind: DraftRunFailureKind,
    pub message: String,
    /// The R call that raised, when the runner named one.
    #[schema(required)]
    pub call: Option<String>,
    pub traceback: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DraftRunResponse {
    /// True when `results` and the rest of the run fields are present; false when `failure` is.
    pub ran: bool,
    #[serde(flatten)]
    #[schema(required)]
    pub run: Option<DraftRunResults>,
    /// Why the run ended without results. A failure here is a finding about the draft, not a
    /// failed request, so it travels at 200 next to `lint`.
    #[schema(required)]
    pub failure: Option<DraftRunFailure>,
    /// Carries the runner the request reached; the version fields are null, nothing here is
    /// stored.
    pub tool_version: ToolVersionRef,
    /// What the save path would say about this script. Findings neither stop a draft from running
    /// nor depend on it running: the runner container is the boundary either way, and an author
    /// mid-edit wants everything that is wrong in one answer. `POST /tool_scripts/{id}/versions`
    /// still refuses to store them. A constant the table does not hold is reported here and left
    /// out of the values the script receives.
    pub lint: Vec<LintFinding>,
}

/// One formula of a draft set, as the calculation editor holds it: the same fields the stored
/// formula carries, without an id, because the point is to run what is not saved yet.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct DraftFormula {
    pub code: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub units: Option<String>,
    pub formula: String,
    pub ordinal: i32,
    #[serde(default)]
    pub curve_slot: Option<String>,
    #[serde(default)]
    pub per_replicate: Option<String>,
    #[serde(default)]
    pub intermediate: bool,
}

/// One formula of a set-level save. An `id` names a stored formula of this calculation, which the
/// save updates; a formula without one is created, and a stored formula the set leaves out is
/// deleted.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SavedFormula {
    #[serde(default)]
    pub id: Option<Uuid>,
    pub code: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub units: Option<String>,
    pub formula: String,
    #[serde(default)]
    pub description: Option<String>,
    pub ordinal: i32,
    #[serde(default)]
    pub curve_slot: Option<String>,
    #[serde(default)]
    pub per_replicate: Option<String>,
    #[serde(default)]
    pub intermediate: bool,
}

/// Save a formula calculation's whole set, as one version (Q186).
#[derive(Debug, Deserialize, ToSchema)]
pub struct SaveFormulaSetRequest {
    pub formulas: Vec<SavedFormula>,
    /// What happens to the values the version being replaced produced (Q170). `false`, the
    /// default, leaves them on that version. `true` is a correction: every visit the superseded
    /// version produced values at is recomputed under the new one.
    #[serde(default)]
    pub migrate_stored: bool,
}

/// What one set-level save wrote.
#[derive(Debug, Serialize, ToSchema)]
pub struct SaveFormulaSetResponse {
    /// The version the save minted, or the one it matched: a save that changes nothing mints none.
    #[schema(required)]
    pub version_id: Option<Uuid>,
    #[schema(required)]
    pub version_no: Option<i32>,
    pub created: usize,
    pub updated: usize,
    pub deleted: usize,
    /// Whether the migration of the superseded version's values was enqueued.
    pub migrated: bool,
    /// One entry per formula this save turned from an output into a step. Publication stops and
    /// nothing is deleted, so the entry says what stays behind and who still reads it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub given_up: Vec<GivenUpOutput>,
}

/// A catalog parameter a formula published until this save ticked it as a step.
#[derive(Debug, Serialize, ToSchema)]
pub struct GivenUpOutput {
    /// The formula's code, which is the catalog row's code too.
    pub code: String,
    pub parameter_id: Uuid,
    /// Readings stored under the parameter. They stay, and ticking the formula back as an output
    /// publishes them again.
    pub readings_retained: i64,
    /// The formulas that read the parameter, by code: each of them now reads a value nothing
    /// refreshes.
    pub read_by: Vec<String>,
    /// The sites holding a slot of the parameter, by name.
    pub sites: Vec<String>,
}

/// Run a formula calculation's unsaved formula set at a visit. The formulas replace the stored
/// set for this run only; nothing is written.
#[derive(Debug, Deserialize, ToSchema)]
pub struct FormulaDraftRunRequest {
    pub formulas: Vec<DraftFormula>,
    /// The calculate request body: `site_id`, `collected_at`, replicate lists and any value
    /// overriding what the visit holds.
    #[serde(default)]
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub inputs: Option<serde_json::Value>,
    /// Constant values in place of the catalog; omit to read the catalog.
    #[serde(default)]
    #[schema(value_type = Option<std::collections::HashMap<String, f64>>)]
    pub constants: Option<serde_json::Map<String, serde_json::Value>>,
}

/// What the formula set produced, present only when the run reached the end.
#[derive(Debug, Serialize, ToSchema)]
pub struct FormulaDraftRunResults {
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub results: serde_json::Value,
    /// Outputs the run did not produce, each with its reason.
    pub skipped: Vec<serde_json::Value>,
    pub inputs_used: Vec<String>,
    pub inputs_ignored: Vec<String>,
    #[schema(value_type = std::collections::HashMap<String, f64>)]
    pub constants: serde_json::Value,
    pub curves: Vec<CurveSnapshot>,
    /// The site properties the run read, as `{property, param, value}`.
    pub site_inputs: Vec<serde_json::Value>,
    /// The visit's stored values the run read, as `{param, parameter_code, parameter_id, value}`.
    pub event_inputs: Vec<serde_json::Value>,
    /// Each formula as it was evaluated, with the values it read per cell.
    pub trace: Vec<TraceStep>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct FormulaDraftRunResponse {
    /// True when the run fields are present; false when `failure` is.
    pub ran: bool,
    #[serde(flatten)]
    #[schema(required)]
    pub run: Option<FormulaDraftRunResults>,
    #[schema(required)]
    pub failure: Option<DraftRunFailure>,
    /// The manifest the formula set implies: its params, outputs, constants, curve slots and the
    /// site and event inputs, in the shape `GET /tools` serves.
    #[schema(value_type = Object)]
    pub manifest: serde_json::Value,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct InspectScriptRequest {
    pub script: String,
    /// The function the runner would call. Defaults to `tool`.
    #[serde(default)]
    pub entry_function: Option<String>,
    /// A manifest to set the inspection against. Supplying one adds `reconciliation` to the
    /// response; nothing here reads or writes a stored manifest.
    #[serde(default)]
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub manifest: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InspectScriptResponse {
    #[serde(flatten)]
    pub inspection: ScriptInspection,
    /// Present only when the request carried a manifest.
    #[schema(required)]
    pub reconciliation: Option<ManifestReconciliation>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CaseResult {
    pub name: String,
    pub passed: bool,
    /// Per-key mismatches: expected vs got, or the missing/unexpected key.
    pub failures: Vec<String>,
    /// The runner's error text when the script itself failed.
    #[schema(required)]
    pub error: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ValidateResponse {
    pub passed: bool,
    pub cases: Vec<CaseResult>,
    #[schema(required)]
    pub validated_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ActivateRequest {
    /// What happens to the values the version being replaced produced (Q170). `false`, the
    /// default, leaves them on that version: this activation is a new method, and the history
    /// stands as it was computed. `true` is a correction: every visit the superseded version
    /// produced values at is recomputed under the new one.
    #[serde(default)]
    pub migrate_stored: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ActivateResponse {
    #[serde(flatten)]
    pub script: ToolScript,
    /// What the manifest's catalog references amount to now, re-checked against the catalog as it
    /// stands rather than as it stood when the version was saved. A parameter deleted since then
    /// leaves a dead `parameter_id` behind, and this is where an operator can see it.
    pub lint: Vec<LintFinding>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ActivationRecord {
    #[schema(required)]
    pub from_version_no: Option<i32>,
    pub to_version_no: i32,
    #[schema(required)]
    pub activated_by: Option<String>,
    pub activated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Debug)]
pub struct EventContext {
    pub id: Uuid,
    pub site_id: Uuid,
    pub collected_at: chrono::DateTime<chrono::Utc>,
}

/// One value the chain would produce at a visit, were the staged cells saved. `value` is absent
/// where the calculation cleared the slot, which a save records as a withdrawal.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct PreviewedValue {
    /// The manifest key the value came out of.
    pub output: String,
    pub parameter_id: Uuid,
    #[schema(required)]
    pub replicate_index: Option<i16>,
    #[schema(required)]
    pub value: Option<f64>,
}

/// What the calculation chain would produce at a visit, given what the operator has typed and not
/// saved. Nothing in it is stored and nothing in it names a run: Save executes the same walk and
/// mints the run the stored values cite (Q212).
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct EventPreview {
    pub site_id: Uuid,
    pub collected_at: chrono::DateTime<chrono::Utc>,
    pub outputs: Vec<PreviewedValue>,
    /// One entry per calculation that ran, in the order it ran, with the inputs, constants,
    /// curves and versions it consumed.
    pub calculations: Vec<ToolCalculation>,
    /// Calculations that did not run at this visit, as `(tool, reason)`.
    pub skipped: Vec<(String, String)>,
    /// Calculations the site declares nothing for.
    pub not_applicable: Vec<String>,
    /// Calculations whose stored run already consumed exactly this: the slot keeps the value it
    /// holds.
    pub unchanged: Vec<String>,
}

pub struct RecomputeOutcome {
    pub tools_run: usize,
    pub readings_written: usize,
    /// Open missing/stale findings closed because this run rewrote their output.
    pub findings_closed: usize,
    /// Stored outputs withdrawn because the script computed them as NA (M23): the portal blanked
    /// the column, and here the stamp is reversible.
    pub readings_withdrawn: usize,
    pub skipped: Vec<(String, String)>,
    /// `skipped_output` findings raised because a step did not run and its outputs are absent.
    pub findings_raised: usize,
    /// Tools the site never declared: it holds no slot for what they read and none for what they
    /// write, so they do not apply here at all (Q98, narrowed by Q193). Distinct from `skipped`,
    /// which is an input that did not resolve on a tool that does apply.
    pub not_applicable: Vec<String>,
    /// Output slots minted at the site because the run published where the site declared the
    /// inputs and not the output (Q193). Each carries `needs_review` until a manager confirms it.
    pub slots_minted: usize,
    /// Tools whose prior run at this event consumed exactly what a fresh run would, under the
    /// same script version, with its outputs still served: left alone, no run minted.
    pub unchanged: Vec<String>,
}

/// The scope a recompute job covers when it names no single event.
#[derive(Debug, Clone, Default)]
pub struct RecomputeScope {
    pub site_id: Option<Uuid>,
    pub start: Option<chrono::DateTime<chrono::Utc>>,
    pub end: Option<chrono::DateTime<chrono::Utc>>,
    pub only_findings: bool,
    /// Hold the findings arm to the ones one calculation raised. A narrowing, never a bound: a
    /// calculation names no window, so it cannot stand as a scope on its own.
    pub calculation: Option<String>,
    /// A superseded script version, as the author's migrate arm names it. A bound, unlike
    /// `calculation`: the visits a version produced values at are a finite set its provenance
    /// names, and it stops growing the moment the version stops being active.
    pub version: Option<Uuid>,
    /// One constant by name: the visits whose stored provenance records it as an input. A bound in
    /// its own right, because a corrected constant names exactly the visits computed from the old
    /// value and no window a person could supply would be as precise.
    pub constant: Option<String>,
}

impl RecomputeScope {
    /// A scope names a site, a range, or holds itself to open findings. Nothing else is
    /// accepted: "every visit there is" is not a repair, it is a global recompute (D6).
    #[must_use]
    pub fn is_bounded(&self) -> bool {
        self.site_id.is_some()
            || self.start.is_some()
            || self.end.is_some()
            || self.only_findings
            || self.version.is_some()
            || self.constant.is_some()
    }

    /// The SELECT of visit ids this scope covers, oldest first. `portal_sync` visits are never
    /// in scope (Q41); `only_findings` holds the set to visits with an open event finding, and a
    /// `calculation` beside it to the findings that calculation raised.
    #[must_use]
    pub fn events_sql(&self) -> (String, Vec<sea_orm::Value>) {
        let mut sql = format!(
            "SELECT ce.id FROM collection_events ce WHERE ce.source <> '{portal_sync}'",
            portal_sync = crate::routes::private::collection_events::service::PORTAL_SYNC
        );
        let mut binds: Vec<sea_orm::Value> = Vec::new();
        if let Some(site_id) = self.site_id {
            binds.push(site_id.into());
            sql.push_str(&format!(" AND ce.site_id = ${}", binds.len()));
        }
        if let Some(start) = self.start {
            binds.push(sea_orm::prelude::DateTimeWithTimeZone::from(start).into());
            sql.push_str(&format!(" AND ce.collected_at >= ${}", binds.len()));
        }
        if let Some(end) = self.end {
            binds.push(sea_orm::prelude::DateTimeWithTimeZone::from(end).into());
            sql.push_str(&format!(" AND ce.collected_at <= ${}", binds.len()));
        }
        if let Some(version) = self.version {
            binds.push(version.to_string().into());
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM readings r \
                      WHERE r.collection_event_id = ce.id \
                        AND r.provenance -> 'tool_version' ->> 'script_version_id' = ${})",
                binds.len()
            ));
        }
        if let Some(name) = &self.constant {
            binds.push(name.clone().into());
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM readings r \
                      WHERE r.collection_event_id = ce.id \
                        AND jsonb_exists(r.provenance -> 'constants', ${}))",
                binds.len()
            ));
        }
        if self.only_findings {
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM replicate_audit_holds h \
                      WHERE h.stream_id IS NULL AND h.status = '{pending}' \
                        AND h.kind IN {kinds} \
                        AND h.site_id = ce.site_id AND h.group_time = ce.collected_at{tool})",
                pending = HoldStatus::Pending.as_str(),
                kinds = HoldKind::sql_list(&HoldKind::EVENT_AUDIT),
                tool = if let Some(name) = &self.calculation {
                    binds.push(name.clone().into());
                    format!(" AND h.tool = ${}", binds.len())
                } else {
                    String::new()
                }
            ));
        }
        sql.push_str(" ORDER BY ce.collected_at");
        (sql, binds)
    }
}

pub struct AuditCounts {
    pub events_audited: usize,
    pub missing: usize,
    pub stale: usize,
    pub superseded: usize,
}

/// `event_recompute`: the chain executor over one collection event.
pub struct EventRecompute;

/// `event_audit`: the missing/stale report over one event, one site, or everything.
pub struct EventAudit;

pub mod run {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    /// One execution of a calculation: what went in, what came out, and the version that decided.
    /// A row is minted only by `/tools/{name}/calculate` and never edited, which is what lets a
    /// saved reading point at one as its provenance (D9). `context` records the calculation
    /// context the server resolved, `source` the path that minted it (interactive, csv_import,
    /// chain).
    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
    )]
    #[sea_orm(table_name = "tool_runs")]
    #[crudcrate(
        api_struct = "ToolRun",
        name_singular = "tool_run",
        name_plural = "tool_runs",
        generate_router,
        routes(read),
        derive_partial_eq
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable, sortable)]
        pub tool_name: String,
        /// The version that ran, as the provenance blob carries it.
        #[sea_orm(column_type = "JsonBinary")]
        pub tool_version: serde_json::Value,
        #[sea_orm(column_type = "JsonBinary")]
        #[crudcrate(exclude(list))]
        pub inputs: serde_json::Value,
        #[sea_orm(column_type = "JsonBinary")]
        #[crudcrate(exclude(list))]
        pub constants: serde_json::Value,
        #[sea_orm(column_type = "JsonBinary")]
        #[crudcrate(exclude(list))]
        pub curves: serde_json::Value,
        #[sea_orm(column_type = "JsonBinary")]
        #[crudcrate(exclude(list))]
        pub outputs: serde_json::Value,
        #[crudcrate(filterable, sortable)]
        pub created_by: String,
        #[crudcrate(exclude(create, update), sortable)]
        pub created_at: chrono::DateTime<chrono::Utc>,
        /// The resolved calculation context: site, collected_at, and the station and event inputs
        /// the server read. Null on a run that named none.
        #[sea_orm(column_type = "JsonBinary", nullable)]
        #[crudcrate(exclude(list))]
        pub context: Option<serde_json::Value>,
        /// The visit the run was computed at. The same two values `context` carries, written from
        /// the same resolution, as columns a list can be filtered and ordered by: "the runs at
        /// this visit" is a query, not a scan of the blobs.
        #[crudcrate(filterable, sortable)]
        pub site_id: Option<Uuid>,
        #[crudcrate(filterable, sortable)]
        pub collected_at: Option<chrono::DateTime<chrono::Utc>>,
        /// Which path minted the run: `interactive`, `csv_import` or `chain`.
        #[crudcrate(filterable, sortable)]
        pub source: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}
