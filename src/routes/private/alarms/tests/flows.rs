use super::*;

#[tokio::test]
async fn test_a_hook_outside_a_request_reconciles_on_the_spot() {
    assert!(!record_owed());
}

#[tokio::test]
async fn test_every_hook_of_one_request_owes_one_reconcile() {
    let owed = Arc::new(AtomicBool::new(false));
    RECONCILE_OWED
        .scope(owed.clone(), async {
            // One per deleted row of a batch, all of them asking for the same global pass.
            for _ in 0..60 {
                assert!(record_owed());
            }
        })
        .await;
    assert!(owed.load(Ordering::Relaxed));
}

#[test]
fn a_job_with_no_tunables_refuses_every_key() {
    let sweep = AlarmSweep {
        interval_seconds: 60,
    };
    assert!(sweep.validate(&serde_json::json!({})).is_ok());
    let err = sweep
        .validate(&serde_json::json!({ "retention_days": 7 }))
        .unwrap_err();
    assert!(err.contains("no tunables"), "{err}");
    assert!(err.contains("retention_days"), "{err}");
}
