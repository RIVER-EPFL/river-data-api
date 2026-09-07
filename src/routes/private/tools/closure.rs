//! Which calculations a set of parameters feeds, and what those calculations rewrite.
//!
//! The graph is the manifests: a tool reads a parameter through an `event_inputs` entry and
//! writes the parameters its outputs resolve to. The closure over that graph is what a write
//! path reports before it commits (which script this edit re-runs, which columns move) and what
//! the reactive hook uses to decide whether a visit has anything to recompute.

use std::collections::HashMap;

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::AppResult;

use super::chain::dependency_order;
use super::engine;

#[derive(Debug, Clone, Serialize, ToSchema)]
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
}

/// The global parameters a subject moves.
///
/// Metadata only. A calibration's parameters come from its own declaration and its instrument's
/// deployments rather than from a scan of the readings it corrected: the question is asked before
/// an edit, on a page that must answer in one round trip, and the two agree wherever the
/// attribution is right.
pub async fn parameters_of(db: &DatabaseConnection, subject: &Subject) -> AppResult<Vec<Uuid>> {
    let (sql, values): (&str, Vec<sea_orm::Value>) = match subject {
        Subject::Parameters(ids) => return Ok(ids.clone()),
        Subject::Calibration(id) => (
            "SELECT DISTINCT p FROM (
               SELECT c.parameter_id AS p FROM sensor_calibrations c WHERE c.id = $1
               UNION ALL
               SELECT d.parameter_id FROM sensor_deployments d
                 JOIN sensor_calibrations c ON c.sensor_id = d.sensor_id
                WHERE c.id = $1 AND c.parameter_id IS NULL
             ) q WHERE p IS NOT NULL",
            vec![(*id).into()],
        ),
        Subject::Slot(id) => (
            "SELECT parameter_id AS p FROM site_parameters WHERE id = $1",
            vec![(*id).into()],
        ),
        // A replicate of a group is the same parameter as its siblings, so the index the caller
        // holds the reading by does not narrow the answer.
        Subject::Reading { stream_id, .. } => (
            "SELECT sp.parameter_id AS p
               FROM data_streams s
               JOIN site_parameters sp ON sp.id = s.site_parameter_id
              WHERE s.id = $1",
            vec![(*stream_id).into()],
        ),
        Subject::Calculation(name) => (
            "SELECT DISTINCT out.id AS p
               FROM tool_scripts s
               JOIN tool_script_versions v ON v.id = s.active_version_id
               CROSS JOIN LATERAL jsonb_array_elements(COALESCE(v.manifest->'outputs', '[]'::jsonb)) o
               JOIN parameters out
                 ON LOWER(out.code) = LOWER(o->>'suggested_parameter_code')
              WHERE s.name = $1",
            vec![name.clone().into()],
        ),
    };
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    let mut ids = Vec::with_capacity(rows.len());
    for row in &rows {
        ids.push(row.try_get::<Uuid>("", "p")?);
    }
    Ok(ids)
}

/// [`calculations_fed_by`] for any subject.
pub async fn calculations_fed_by_subject(
    db: &DatabaseConnection,
    subject: &Subject,
) -> AppResult<Vec<CalculationImpact>> {
    let ids = parameters_of(db, subject).await?;
    calculations_fed_by(db, &ids).await
}

/// Every enabled calculation that reads one of `parameter_ids` at a visit, directly or through a
/// calculation downstream of it, in the order the chain would run them. Empty when no calculation
/// reads any of them, which is the common case for a logger parameter.
pub async fn calculations_fed_by(
    db: &DatabaseConnection,
    parameter_ids: &[Uuid],
) -> AppResult<Vec<CalculationImpact>> {
    if parameter_ids.is_empty() {
        return Ok(Vec::new());
    }
    let tools = engine::list_active_tools(db).await?;
    if tools.iter().all(|t| t.manifest.event_inputs.is_empty()) {
        return Ok(Vec::new());
    }
    let catalog = engine::load_parameter_catalog(db, tools.iter().map(|t| &t.manifest)).await?;
    let order = dependency_order(&tools, &catalog)?;

    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, code FROM parameters WHERE id = ANY($1)",
            [parameter_ids.to_vec().into()],
        ))
        .await?;
    let touched: Vec<ImpactParameter> = rows
        .iter()
        .map(|row| {
            Ok(ImpactParameter {
                parameter_id: row.try_get("", "id")?,
                parameter_code: row.try_get("", "code")?,
            })
        })
        .collect::<AppResult<_>>()?;
    let mut impacts = fed_closure(&tools, &catalog, &order, &touched);
    impacts.extend(derived_fed_by(db, &touched).await?);
    Ok(impacts)
}

/// One standalone derived definition as an edge of the same graph: what it reads and what it
/// writes.
///
/// A derived parameter attached to a calculation is already in the manifest graph, because a
/// formula calculation presents one. A standalone definition (`tool_script_id IS NULL`) is the
/// continuous kind the derived job and the janitor serve, and it has no manifest, so its
/// dependants were invisible to the closure entirely.
struct DerivedEdge {
    code: String,
    label: String,
    reads: Vec<String>,
    output: Option<ImpactParameter>,
}

async fn derived_edges(db: &DatabaseConnection) -> AppResult<Vec<DerivedEdge>> {
    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT d.code, d.name, out.id AS output_id, out.code AS output_code,
                    COALESCE(
                      (SELECT jsonb_agg(p.code)
                         FROM derived_parameter_sources src
                         JOIN parameters p ON p.id = src.parameter_id
                        WHERE src.derived_definition_id = d.id),
                      '[]'::jsonb) AS reads
               FROM derived_parameter_definitions d
               LEFT JOIN parameters out ON out.id = d.output_parameter_id
              WHERE d.tool_script_id IS NULL
              ORDER BY d.code"
                .to_string(),
        ))
        .await?;
    let mut edges = Vec::with_capacity(rows.len());
    for row in &rows {
        let reads: serde_json::Value = row.try_get("", "reads")?;
        let output_id: Option<Uuid> = row.try_get("", "output_id")?;
        let output_code: Option<String> = row.try_get("", "output_code")?;
        edges.push(DerivedEdge {
            code: row.try_get("", "code")?,
            label: row.try_get("", "name")?,
            reads: reads
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_lowercase))
                        .collect()
                })
                .unwrap_or_default(),
            output: match (output_id, output_code) {
                (Some(parameter_id), Some(parameter_code)) => Some(ImpactParameter {
                    parameter_id,
                    parameter_code,
                }),
                _ => None,
            },
        });
    }
    Ok(edges)
}

/// The standalone derived definitions a touched set feeds, in the same shape a calculation
/// answers in. The walk repeats until nothing new is reachable, so a derived parameter feeding
/// another is followed however the definitions happen to be ordered.
async fn derived_fed_by(
    db: &DatabaseConnection,
    touched: &[ImpactParameter],
) -> AppResult<Vec<CalculationImpact>> {
    let edges = derived_edges(db).await?;
    if edges.is_empty() {
        return Ok(Vec::new());
    }
    Ok(derived_closure(&edges, touched))
}

fn derived_closure(edges: &[DerivedEdge], touched: &[ImpactParameter]) -> Vec<CalculationImpact> {
    let mut reachable: HashMap<String, Vec<ImpactParameter>> = HashMap::new();
    for p in touched {
        reachable.insert(p.parameter_code.to_lowercase(), vec![p.clone()]);
    }
    let mut impacts: Vec<CalculationImpact> = Vec::new();
    // A definition reading another's output is followed by re-walking until the reachable set
    // stops growing: the definitions carry no order of their own, unlike the manifest tools.
    loop {
        let before = impacts.len();
        for edge in edges {
            if impacts.iter().any(|i| i.tool == edge.code) {
                continue;
            }
            let mut reads: Vec<ImpactParameter> = Vec::new();
            for code in &edge.reads {
                for root in reachable.get(code).into_iter().flatten() {
                    if !reads.iter().any(|x| x.parameter_id == root.parameter_id) {
                        reads.push(root.clone());
                    }
                }
            }
            if reads.is_empty() {
                continue;
            }
            if let Some(output) = &edge.output {
                reachable
                    .entry(output.parameter_code.to_lowercase())
                    .or_default()
                    .extend(reads.iter().cloned());
            }
            impacts.push(CalculationImpact {
                tool: edge.code.clone(),
                label: edge.label.clone(),
                reads,
                outputs: edge.output.iter().cloned().collect(),
            });
        }
        if impacts.len() == before {
            return impacts;
        }
    }
}

/// The closure walk itself: with tools in run order, a tool is fed when an `event_input` names a
/// touched parameter or an output of a tool already fed; its outputs then count as reachable for
/// the tools after it.
pub fn fed_closure(
    tools: &[engine::ActiveTool],
    catalog: &engine::ParameterCatalog,
    order: &[usize],
    touched: &[ImpactParameter],
) -> Vec<CalculationImpact> {
    // Reachable parameter code → the touched parameters it descends from.
    let mut reachable: HashMap<String, Vec<ImpactParameter>> = HashMap::new();
    for p in touched {
        reachable.insert(p.parameter_code.to_lowercase(), vec![p.clone()]);
    }

    let mut impacts = Vec::new();
    for &i in order {
        let tool = &tools[i];
        let mut reads: Vec<ImpactParameter> = Vec::new();
        for e in &tool.manifest.event_inputs {
            if let Some(roots) = reachable.get(&e.parameter_code.to_lowercase()) {
                for r in roots {
                    if !reads.iter().any(|x| x.parameter_id == r.parameter_id) {
                        reads.push(r.clone());
                    }
                }
            }
        }
        if reads.is_empty() {
            continue;
        }
        let outputs: Vec<ImpactParameter> = tool
            .manifest
            .outputs
            .iter()
            .filter_map(|o| catalog.resolve(o))
            .map(|p| ImpactParameter {
                parameter_id: p.id,
                parameter_code: p.code.clone(),
            })
            .collect();
        for o in &outputs {
            reachable
                .entry(o.parameter_code.to_lowercase())
                .or_default()
                .extend(reads.iter().cloned());
        }
        impacts.push(CalculationImpact {
            tool: tool.name.clone(),
            label: tool.label.clone(),
            reads,
            outputs,
        });
    }
    impacts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::private::tools::chain::dependency_order;

    fn tool(name: &str, reads: &[&str], writes: &str) -> engine::ActiveTool {
        let manifest = engine::parse_manifest(&serde_json::json!({
            "label": name,
            "params": reads.iter().map(|r| serde_json::json!({
                "name": r, "label": r, "kind": "number", "required": true
            })).collect::<Vec<_>>(),
            "event_inputs": reads.iter().map(|r| serde_json::json!({
                "param": r, "parameter_code": r
            })).collect::<Vec<_>>(),
            "outputs": [{ "key": "out", "label": writes, "suggested_parameter_code": writes }],
        }))
        .expect("manifest");
        let mut t = engine::ActiveTool::draft(
            "tool <- function(i, c, k) list()".into(),
            "tool".into(),
            manifest,
            String::new(),
        );
        t.name = name.to_string();
        t
    }

    fn param(code: &str) -> ImpactParameter {
        // A stable id per code, so the catalog and the touched set agree without a database.
        let mut bytes = [0u8; 16];
        bytes[0] = code.as_bytes()[0];
        ImpactParameter {
            parameter_id: Uuid::from_bytes(bytes),
            parameter_code: code.to_string(),
        }
    }

    fn walk(tools: Vec<engine::ActiveTool>, touched: &[&str]) -> Vec<CalculationImpact> {
        let codes = ["A", "B", "C", "X", "Y"];
        let rows: Vec<(Uuid, &str)> = codes.iter().map(|c| (param(c).parameter_id, *c)).collect();
        let catalog = engine::ParameterCatalog::with_codes(&rows);
        let order = dependency_order(&tools, &catalog).expect("acyclic");
        let touched: Vec<ImpactParameter> = touched.iter().map(|c| param(c)).collect();
        fed_closure(&tools, &catalog, &order, &touched)
    }

    #[test]
    fn a_tool_reading_the_touched_parameter_is_fed_with_its_outputs() {
        let fed = walk(vec![tool("b", &["A"], "B")], &["A"]);
        assert_eq!(fed.len(), 1);
        assert_eq!(fed[0].tool, "b");
        assert_eq!(fed[0].reads[0].parameter_code, "A");
        assert_eq!(fed[0].outputs[0].parameter_code, "B");
    }

    #[test]
    fn a_tool_downstream_of_a_fed_tool_is_fed_through_its_output() {
        let fed = walk(vec![tool("c", &["B"], "C"), tool("b", &["A"], "B")], &["A"]);
        let names: Vec<&str> = fed.iter().map(|f| f.tool.as_str()).collect();
        assert_eq!(names, vec!["b", "c"], "run order, producer first");
        assert_eq!(
            fed[1].reads[0].parameter_code, "A",
            "traced back to the touched root"
        );
    }

    #[test]
    fn a_tool_reading_an_untouched_parameter_is_not_fed() {
        assert!(walk(vec![tool("y", &["X"], "Y")], &["A"]).is_empty());
    }

    #[test]
    fn two_touched_parameters_read_by_one_tool_are_both_listed_once() {
        let fed = walk(vec![tool("c", &["A", "B"], "C")], &["A", "B", "A"]);
        assert_eq!(fed.len(), 1);
        let mut reads: Vec<&str> = fed[0]
            .reads
            .iter()
            .map(|r| r.parameter_code.as_str())
            .collect();
        reads.sort_unstable();
        assert_eq!(reads, vec!["A", "B"]);
    }

    #[test]
    fn matching_is_case_insensitive_like_the_catalog_index() {
        let fed = walk(vec![tool("b", &["a"], "B")], &["A"]);
        assert_eq!(fed.len(), 1);
    }

    fn edge(code: &str, reads: &[&str], writes: Option<&str>) -> DerivedEdge {
        DerivedEdge {
            code: code.to_string(),
            label: code.to_uppercase(),
            reads: reads.iter().map(|r| r.to_lowercase()).collect(),
            output: writes.map(param),
        }
    }

    #[test]
    fn a_standalone_derived_definition_reading_the_touched_parameter_is_reported() {
        let fed = derived_closure(&[edge("b", &["A"], Some("B"))], &[param("A")]);
        assert_eq!(fed.len(), 1);
        assert_eq!(fed[0].tool, "b");
        assert_eq!(fed[0].outputs[0].parameter_code, "B");
    }

    /// The definitions carry no order of their own, so a consumer declared before its producer is
    /// still followed.
    #[test]
    fn a_derived_definition_reading_another_s_output_is_followed_whatever_the_order() {
        let fed = derived_closure(
            &[edge("c", &["B"], Some("C")), edge("b", &["A"], Some("B"))],
            &[param("A")],
        );
        let mut names: Vec<&str> = fed.iter().map(|f| f.tool.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["b", "c"]);
        assert_eq!(
            fed.iter().find(|f| f.tool == "c").unwrap().reads[0].parameter_code,
            "A",
            "traced back to the touched root"
        );
    }

    #[test]
    fn a_derived_definition_reading_nothing_touched_is_not_reported() {
        assert!(derived_closure(&[edge("y", &["X"], Some("Y"))], &[param("A")]).is_empty());
    }

    /// A definition with no output parameter yet is still reported: it reads the touched value, so
    /// an operator has to know it runs again, even though nothing downstream can read it.
    #[test]
    fn a_definition_with_no_output_is_reported_with_none() {
        let fed = derived_closure(&[edge("b", &["A"], None)], &[param("A")]);
        assert_eq!(fed.len(), 1);
        assert!(fed[0].outputs.is_empty());
    }

    #[test]
    fn a_cycle_among_definitions_terminates_rather_than_walking_forever() {
        let fed = derived_closure(
            &[edge("b", &["A", "C"], Some("B")), edge("c", &["B"], Some("C"))],
            &[param("A")],
        );
        assert_eq!(fed.len(), 2, "each definition is reported once: {fed:?}");
    }
}
