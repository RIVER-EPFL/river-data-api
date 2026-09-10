use serde_json::json;

use super::validate_command;

#[test]
fn test_validate_command_accepts_every_command_that_needs_no_payload() {
    for c in [
        "trigger_sync",
        "trigger_full_sync",
        "pause",
        "resume",
        "source_audit",
    ] {
        assert_eq!(validate_command(c, None), Ok(()), "{c}");
    }
}

#[test]
fn test_validate_command_rejects_an_unknown_name() {
    let err = validate_command("full_sync", None).unwrap_err();
    assert!(err.contains("Invalid command 'full_sync'"), "{err}");
    assert!(
        err.contains("resync_streams"),
        "lists every valid name: {err}"
    );
    assert!(
        err.contains("source_audit"),
        "lists every valid name: {err}"
    );
}

#[test]
fn test_validate_command_resync_needs_source_keys() {
    let keys = json!({"source_keys": ["FP3:DOC_avg_ppb:reps"]});
    assert_eq!(validate_command("resync_streams", Some(&keys)), Ok(()));
    let with_overwrite = json!({"source_keys": ["FP3:DOC_avg_ppb:reps"], "overwrite": false});
    assert_eq!(
        validate_command("resync_streams", Some(&with_overwrite)),
        Ok(())
    );

    assert!(validate_command("resync_streams", None).is_err());
    assert!(validate_command("resync_streams", Some(&json!({}))).is_err());
    assert!(validate_command("resync_streams", Some(&json!({"source_keys": []}))).is_err());
    assert!(validate_command("resync_streams", Some(&json!({"source_keys": "FP3"}))).is_err());
    assert!(validate_command("resync_streams", Some(&json!({"source_keys": [1]}))).is_err());
}
