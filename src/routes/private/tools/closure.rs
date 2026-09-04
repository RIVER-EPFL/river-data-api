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
    Ok(fed_closure(&tools, &catalog, &order, &touched))
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
}
