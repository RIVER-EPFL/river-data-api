use super::{JobReport, janitor_outcome};

fn failed_steps(report: &JobReport) -> serde_json::Value {
    report.to_value()["scope"]["failed_steps"].clone()
}

#[test]
fn test_janitor_outcome_clean_tick_completes() {
    let outcome = janitor_outcome([
        ("gap_fill", Ok(())),
        ("curve_drift", Ok(())),
        ("job_prune", Ok(())),
    ]);

    assert_eq!(outcome.result(7).unwrap(), 7);
    assert_eq!(
        failed_steps(&outcome.report_into(JobReport::new())),
        serde_json::json!([])
    );
}

/// Expected behaviour: a step failing does not stop the ones after it from being counted, and the
/// tick is a failed run naming the step, so retry and the job_failed notice reach it.
#[test]
fn test_janitor_outcome_one_failed_step_fails_the_run() {
    let outcome = janitor_outcome([
        ("gap_fill", Ok(())),
        (
            "curve_drift",
            Err("canceling statement due to statement timeout".into()),
        ),
        ("job_prune", Ok(())),
    ]);

    let err = outcome.result(3).unwrap_err().to_string();
    assert!(err.contains("curve_drift"));
    assert!(err.contains("statement timeout"));
    assert!(!err.contains("job_prune"));
    assert_eq!(
        failed_steps(&outcome.report_into(JobReport::new())),
        serde_json::json!([{
            "step": "curve_drift",
            "error": "canceling statement due to statement timeout",
        }])
    );
}

#[test]
fn test_janitor_outcome_every_failure_is_named_in_order() {
    let outcome = janitor_outcome([
        ("curve_drift", Err("a".into())),
        ("import_session_prune", Ok(())),
        ("job_prune", Err("b".into())),
    ]);

    let err = outcome.result(0).unwrap_err().to_string();
    assert!(err.find("curve_drift").unwrap() < err.find("job_prune").unwrap());
    let steps = failed_steps(&outcome.report_into(JobReport::new()));
    assert_eq!(steps[0]["step"], "curve_drift");
    assert_eq!(steps[1]["step"], "job_prune");
    assert_eq!(steps.as_array().unwrap().len(), 2);
}

#[test]
fn test_janitor_outcome_no_steps_completes() {
    let outcome = janitor_outcome(Vec::new());
    assert_eq!(outcome.result(0).unwrap(), 0);
}
