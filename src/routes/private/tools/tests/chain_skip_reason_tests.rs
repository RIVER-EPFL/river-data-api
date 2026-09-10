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
