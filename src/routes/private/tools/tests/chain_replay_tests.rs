use super::{ActiveTool, EventContext, body_for_run, disagrees};
use crate::routes::private::tools::models::Engine;
use crate::routes::private::tools::models::Manifest;
use uuid::Uuid;

fn tool(manifest: serde_json::Value) -> ActiveTool {
    let manifest: Manifest = serde_json::from_value(manifest).expect("manifest parses");
    ActiveTool {
        script_id: Uuid::from_u128(1),
        name: "pco2".to_string(),
        label: "pCO2".to_string(),
        description: None,
        version_id: Uuid::from_u128(2),
        version_no: 1,
        script: String::new(),
        entry_function: "tool".to_string(),
        content_hash: "hash".to_string(),
        manifest,
        engine: Engine::Script,
        formulas: Vec::new(),
    }
}

fn event() -> EventContext {
    EventContext {
        id: Uuid::from_u128(3),
        site_id: Uuid::from_u128(4),
        collected_at: chrono::DateTime::from_timestamp(1_772_259_000, 0).expect("representable"),
    }
}

fn manifest_with(extra: serde_json::Value) -> serde_json::Value {
    let mut base = serde_json::json!({
        "label": "pCO2",
        "params": [{ "name": "lab_temp_c", "label": "Lab temperature", "kind": "number" }],
        "outputs": [],
    });
    for (k, v) in extra.as_object().expect("object") {
        base[k] = v.clone();
    }
    base
}

#[test]
fn test_body_for_run_replays_the_prior_run_s_own_inputs() {
    let blob = serde_json::json!({ "inputs": { "lab_temp_c": 21.5, "mode": "db" } });
    let body = body_for_run(
        &tool(manifest_with(serde_json::json!({}))),
        &event(),
        Some(&blob),
    );
    assert_eq!(body["lab_temp_c"], 21.5);
    assert_eq!(body["mode"], "db");
}

// Scenario: a run made under an earlier shape is recomputed.
// Expected behaviour: what the context resolves is dropped from the replayed inputs, so an
// upstream value that has since changed propagates instead of the stored copy winning.
#[test]
fn test_body_for_run_drops_what_the_context_resolves() {
    let manifest = manifest_with(serde_json::json!({
        "params": [
            { "name": "lab_temp_c", "label": "Lab temperature", "kind": "number" },
            { "name": "water_temp_c", "label": "Water temperature", "kind": "number" },
            { "name": "elevation_m", "label": "Elevation", "kind": "number" },
        ],
        "event_inputs": [{ "param": "water_temp_c", "parameter_code": "WTW_Temp_degC_1" }],
        "site_inputs": [{ "property": "altitude_m", "param": "elevation_m" }],
    }));
    let blob = serde_json::json!({
        "inputs": { "lab_temp_c": 21.5, "water_temp_c": 4.0, "elevation_m": 1500 }
    });
    let body = body_for_run(&tool(manifest), &event(), Some(&blob));
    assert_eq!(body["lab_temp_c"], 21.5);
    assert!(
        !body.contains_key("water_temp_c"),
        "the event input is re-resolved"
    );
    assert!(!body.contains_key("elevation_m"), "so is the site input");
}

// A site input with no `param` fills the property's own name, and that is what has to go.
#[test]
fn test_body_for_run_drops_a_site_input_that_names_no_param() {
    let manifest = manifest_with(serde_json::json!({
        "params": [{ "name": "altitude_m", "label": "Altitude", "kind": "number" }],
        "site_inputs": [{ "property": "altitude_m" }],
    }));
    let blob = serde_json::json!({ "inputs": { "altitude_m": 1500 } });
    let body = body_for_run(&tool(manifest), &event(), Some(&blob));
    assert!(!body.contains_key("altitude_m"));
}

#[test]
fn test_body_for_run_states_the_context_over_a_stale_stored_copy() {
    let blob = serde_json::json!({
        "inputs": {
            "site_id": "00000000-0000-0000-0000-0000000000ff",
            "collected_at": "2020-01-01T00:00:00Z"
        }
    });
    let body = body_for_run(
        &tool(manifest_with(serde_json::json!({}))),
        &event(),
        Some(&blob),
    );
    assert_eq!(body["site_id"], serde_json::json!(Uuid::from_u128(4)));
    assert_eq!(body["collected_at"], "2026-02-28T06:10:00Z");
}

#[test]
fn test_body_for_run_with_no_prior_run_carries_the_context_alone() {
    let body = body_for_run(&tool(manifest_with(serde_json::json!({}))), &event(), None);
    assert_eq!(body.len(), 2);
    assert!(body.contains_key("site_id") && body.contains_key("collected_at"));
}

// A blob with no `inputs` object is not a replayable run: nothing is carried forward.
#[test]
fn test_body_for_run_ignores_a_blob_with_no_inputs() {
    let blob = serde_json::json!({ "constants": { "xO2": 0.209446 } });
    let body = body_for_run(
        &tool(manifest_with(serde_json::json!({}))),
        &event(),
        Some(&blob),
    );
    assert_eq!(body.len(), 2);
}

/// The audit's tolerance, at the two edges that decide whether a run is reported at all.
#[test]
fn test_disagrees_at_the_tolerance_and_across_zero() {
    assert!(!disagrees(1.0, 1.0));
    assert!(!disagrees(1.0, 1.0 + 1e-12));
    assert!(disagrees(1.0, 1.000_001));
    // Both sides zero is agreement, not a division by nothing.
    assert!(!disagrees(0.0, 0.0));
    assert!(disagrees(0.0, 1e-6));
}
