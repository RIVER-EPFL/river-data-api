use crate::error::AppError;
use crate::routes::private::tools::flows::skip_reason;

/// Expected behaviour: a tool whose inputs do not resolve, and a tool whose script raises,
/// are both skips. The chain records them and runs on; neither fails the run.
#[test]
fn an_unresolved_input_and_a_script_error_are_both_skips() {
    assert_eq!(
        skip_reason(&AppError::BadRequest("pa is required".to_string())),
        Some("pa is required".to_string())
    );
    assert_eq!(
        skip_reason(&AppError::ToolScriptError {
            message: "division by zero".to_string(),
            call: Some("tool(inputs, constants, curves)".to_string()),
            traceback: vec!["stop(...)".to_string()],
        }),
        Some("script error: division by zero".to_string())
    );
}

#[test]
fn anything_else_fails_the_run() {
    assert!(skip_reason(&AppError::Internal("pool closed".to_string())).is_none());
    assert!(skip_reason(&AppError::NotFound("event".to_string())).is_none());
    assert!(
        skip_reason(&AppError::ServiceUnavailable("runner down".to_string())).is_none(),
        "a runner that cannot be reached is not a tool that cannot be computed"
    );
}

/// The reason a run gave for skipping one output, read from its `skipped` list.
#[test]
fn test_skipped_reason_names_only_the_output_asked_for() {
    let skipped = vec![
        serde_json::json!({ "output": "doc_avg", "reason": "no value for blank" }),
        serde_json::json!({ "output": "doc_sd" }),
    ];
    assert_eq!(
        super::skipped_reason(&skipped, "doc_avg"),
        Some("no value for blank".to_string())
    );
    assert_eq!(super::skipped_reason(&skipped, "doc_sd"), None);
    assert_eq!(super::skipped_reason(&skipped, "dom"), None);
    assert_eq!(super::skipped_reason(&[], "doc_avg"), None);
}

/// Expected behaviour: each reason the resolver, the evaluator and the runner write names what
/// would let the step run: an input, the step it reads, or a fix to its arithmetic or script.
#[test]
fn test_skip_cause_reads_each_reason_the_chain_writes() {
    use super::{SkipCause, skip_cause};
    let inputs =
        |reason: &str| assert_eq!(skip_cause(reason), (SkipCause::Inputs, None), "{reason}");
    inputs("pa is required");
    inputs("missing required field 'alk' for tool 'pco2real'");
    inputs("no value for alk (ALK)");
    inputs("no value for z (site altitude)");
    inputs("curve 'std' was not supplied");
    inputs("no replicate family for doc (mean(doc))");
    inputs("the visit no longer holds what this output was computed from");
    let error = |reason: &str| assert_eq!(skip_cause(reason), (SkipCause::Error, None), "{reason}");
    error("script error: division by zero");
    error("computed as inf, not a finite number");
    assert_eq!(
        skip_cause("no value for doc_blank"),
        (SkipCause::Upstream, Some("doc_blank".to_string()))
    );
    assert_eq!(
        skip_cause("waits on chain_b, which did not run: pa is required"),
        (SkipCause::Upstream, Some("chain_b".to_string()))
    );
    assert_eq!(
        skip_cause("run produced no savable output"),
        (SkipCause::Unknown, None)
    );
}

#[test]
fn test_a_step_waiting_on_a_failed_one_says_which() {
    assert_eq!(
        super::awaited_reason("chain_b", "pa is required"),
        "waits on chain_b, which did not run: pa is required"
    );
}
