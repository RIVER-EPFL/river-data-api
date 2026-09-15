use crate::routes::private::tools::flows::dependency_order;
use crate::routes::private::tools::models::*;
use crate::routes::private::tools::service::*;

fn tool(
    name: &str,
    reads: &[&str],
    writes: &str,
) -> crate::routes::private::tools::models::ActiveTool {
    let manifest = crate::routes::private::tools::models::parse_manifest(&serde_json::json!({
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
    let mut t = crate::routes::private::tools::models::ActiveTool::draft(
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

fn walk(
    tools: Vec<crate::routes::private::tools::models::ActiveTool>,
    touched: &[&str],
) -> Vec<CalculationImpact> {
    let codes = ["A", "B", "C", "X", "Y"];
    let rows: Vec<(Uuid, &str)> = codes.iter().map(|c| (param(c).parameter_id, *c)).collect();
    let catalog = crate::routes::private::tools::service::ParameterCatalog::with_codes(&rows);
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
        &[
            edge("b", &["A", "C"], Some("B")),
            edge("c", &["B"], Some("C")),
        ],
        &[param("A")],
    );
    assert_eq!(fed.len(), 2, "each definition is reported once: {fed:?}");
}

/// A calculation that evaluates over replicate families declares them as `replicates` params and
/// has no event inputs at all: the family is the read, and it names its parameter on the param.
fn replicate_tool(
    name: &str,
    reads: &[&str],
    writes: &str,
) -> crate::routes::private::tools::models::ActiveTool {
    let manifest = crate::routes::private::tools::models::parse_manifest(&serde_json::json!({
        "label": name,
        "params": reads.iter().map(|r| serde_json::json!({
            "name": r, "label": r, "kind": "replicates", "parameter_code": r, "required": false
        })).collect::<Vec<_>>(),
        "outputs": [{ "key": "out", "label": writes, "suggested_parameter_code": writes }],
    }))
    .expect("manifest");
    let mut t = crate::routes::private::tools::models::ActiveTool::draft(
        "tool <- function(i, c, k) list()".into(),
        "tool".into(),
        manifest,
        String::new(),
    );
    t.name = name.to_string();
    t
}

#[test]
fn a_tool_reading_a_replicate_family_is_fed_by_that_parameter() {
    let fed = walk(vec![replicate_tool("b", &["A"], "B")], &["A"]);
    assert_eq!(fed.len(), 1);
    assert_eq!(fed[0].tool, "b");
    assert_eq!(fed[0].reads[0].parameter_code, "A");
    assert_eq!(fed[0].outputs[0].parameter_code, "B");
}

#[test]
fn a_tool_downstream_of_a_replicate_reading_tool_is_ordered_after_it() {
    let fed = walk(
        vec![tool("c", &["B"], "C"), replicate_tool("b", &["A"], "B")],
        &["A"],
    );
    let names: Vec<&str> = fed.iter().map(|f| f.tool.as_str()).collect();
    assert_eq!(names, vec!["b", "c"], "run order, producer first");
    assert_eq!(fed[1].reads[0].parameter_code, "A");
}

#[test]
fn a_tool_reading_a_replicate_family_of_an_untouched_parameter_is_not_fed() {
    assert!(walk(vec![replicate_tool("y", &["X"], "Y")], &["A"]).is_empty());
}

/// The same parameter read both ways at one visit: the family by the calculation that evaluates
/// over the repeats, and the served value by a scalar reader. Both are fed.
#[test]
fn a_family_and_a_scalar_reader_of_one_parameter_are_both_fed() {
    let fed = walk(
        vec![replicate_tool("b", &["A"], "B"), tool("c", &["A"], "C")],
        &["A"],
    );
    let mut names: Vec<&str> = fed.iter().map(|f| f.tool.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["b", "c"]);
    assert!(fed.iter().all(|f| f.reads[0].parameter_code == "A"));
}

/// A manifest declaring both forms of the same read reports the parameter once, not twice.
#[test]
fn a_parameter_read_as_a_family_and_an_event_input_is_listed_once() {
    let manifest = crate::routes::private::tools::models::parse_manifest(&serde_json::json!({
        "label": "b",
        "params": [
            { "name": "reps", "label": "reps", "kind": "replicates", "parameter_code": "A", "required": false },
            { "name": "mean", "label": "mean", "kind": "number", "required": false },
        ],
        "event_inputs": [{ "param": "mean", "parameter_code": "A" }],
        "outputs": [{ "key": "out", "label": "B", "suggested_parameter_code": "B" }],
    }))
    .expect("manifest");
    assert_eq!(manifest.read_codes(), vec!["a".to_string()]);
}
