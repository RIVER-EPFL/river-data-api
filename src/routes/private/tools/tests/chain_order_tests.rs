use super::{blob_fingerprint, dependency_order};
use crate::routes::private::tools::models::ActiveTool;
use crate::routes::private::tools::models::Engine;
use crate::routes::private::tools::service::ParameterCatalog;
use uuid::Uuid;

/// A tool that reads `reads` at the event and writes `writes`, named by catalog code.
fn tool(name: &str, writes: &[&str], reads: &[&str]) -> ActiveTool {
    let manifest = serde_json::json!({
        "label": name,
        "params": reads.iter().map(|code| serde_json::json!({
            "name": code, "label": code, "kind": "number",
        })).collect::<Vec<_>>(),
        "outputs": writes.iter().map(|code| serde_json::json!({
            "key": code, "label": code, "suggested_parameter_code": code,
        })).collect::<Vec<_>>(),
        "event_inputs": reads.iter().map(|code| serde_json::json!({
            "param": code, "parameter_code": code,
        })).collect::<Vec<_>>(),
    });
    ActiveTool {
        script_id: Uuid::new_v4(),
        name: name.to_string(),
        label: name.to_string(),
        description: None,
        version_id: Uuid::new_v4(),
        version_no: 1,
        script: String::new(),
        entry_function: "tool".to_string(),
        content_hash: String::new(),
        manifest: crate::routes::private::tools::models::parse_manifest(&manifest)
            .expect("the manifest parses"),
        engine: Engine::Script,
        parameter_group_id: None,
        formulas: Vec::new(),
    }
}

fn catalog(codes: &[&str]) -> ParameterCatalog {
    let rows: Vec<(Uuid, &str)> = codes.iter().map(|c| (Uuid::new_v4(), *c)).collect();
    ParameterCatalog::with_codes(&rows)
}

#[test]
fn a_producer_orders_ahead_of_its_consumer_whatever_the_declaration_order() {
    let tools = [
        tool("pco2", &["pco2"], &["k_h"]),
        tool("henry", &["k_h"], &["water_temp"]),
    ];
    let order = dependency_order(&tools, &catalog(&["pco2", "k_h", "water_temp"]))
        .expect("the set has an order");
    assert_eq!(order, vec![1, 0]);
}

#[test]
fn tools_that_read_nothing_of_each_other_keep_their_declaration_order() {
    let tools = [
        tool("doc", &["doc"], &["a254"]),
        tool("chla", &["chla"], &["abs_665"]),
    ];
    let order = dependency_order(&tools, &catalog(&["doc", "chla", "a254", "abs_665"]))
        .expect("the set has an order");
    assert_eq!(order, vec![0, 1]);
}

#[test]
fn a_cycle_is_refused_naming_both_tools() {
    let tools = [
        tool("a", &["out_a"], &["out_b"]),
        tool("b", &["out_b"], &["out_a"]),
    ];
    let err = dependency_order(&tools, &catalog(&["out_a", "out_b"]))
        .expect_err("two tools feeding each other have no runnable order");
    let message = err.to_string();
    assert!(message.contains("cycle"), "{message}");
    assert!(message.contains('a') && message.contains('b'), "{message}");
}

#[test]
fn a_tool_reading_its_own_output_orders_rather_than_deadlocking_on_itself() {
    // The self-edge is excluded (a != b). Refusing a calculation that reads what it writes is
    // the formula engine's job, at its own level.
    let tools = [tool("a", &["out_a"], &["out_a"])];
    assert_eq!(
        dependency_order(&tools, &catalog(&["out_a"])).expect("one tool always orders"),
        vec![0]
    );
}

fn blob(version: Uuid, inputs: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "tool_version": { "script_version_id": version.to_string() },
        "inputs": inputs,
        "constants": { "xO2": 1.0 },
        "curves": [],
    })
}

#[test]
fn a_run_fingerprints_the_same_whatever_order_its_inputs_are_written_in() {
    let version = Uuid::new_v4();
    assert_eq!(
        blob_fingerprint(&blob(version, serde_json::json!({ "a": 1.0, "b": 2.0 }))),
        blob_fingerprint(&blob(version, serde_json::json!({ "b": 2.0, "a": 1.0 }))),
    );
}

#[test]
fn a_changed_input_or_a_new_script_version_fingerprints_differently() {
    let version = Uuid::new_v4();
    let base = blob_fingerprint(&blob(version, serde_json::json!({ "a": 1.0 })));
    assert!(base.is_some());
    assert_ne!(
        base,
        blob_fingerprint(&blob(version, serde_json::json!({ "a": 1.5 })))
    );
    assert_ne!(
        base,
        blob_fingerprint(&blob(Uuid::new_v4(), serde_json::json!({ "a": 1.0 })))
    );
}

#[test]
fn a_blob_naming_no_script_version_has_no_fingerprint_and_never_memoises() {
    assert_eq!(blob_fingerprint(&serde_json::json!({ "inputs": {} })), None);
    assert_eq!(
        blob_fingerprint(&serde_json::json!({
            "tool_version": { "script_version_id": "not-a-uuid" }
        })),
        None
    );
}
