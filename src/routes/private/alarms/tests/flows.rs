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

fn at(minute: u32) -> DateTime<Utc> {
    chrono::TimeZone::with_ymd_and_hms(&Utc, 2025, 2, 1, 0, minute, 0).unwrap()
}

#[test]
fn test_match_episodes_updates_the_stored_episode_with_the_same_start() {
    let id = Uuid::new_v4();
    let matched = match_episodes(&[(id, "continuous", at(10))], &[("continuous", at(10))]);
    assert_eq!(
        matched,
        EpisodeMatch {
            stored: vec![Some(id)],
            stale: vec![],
        }
    );
}

#[test]
fn test_match_episodes_inserts_a_new_start_and_reports_the_unclaimed_row() {
    let id = Uuid::new_v4();
    let matched = match_episodes(
        &[(id, "continuous", at(10))],
        &[("continuous", at(20)), ("continuous", at(30))],
    );
    assert_eq!(matched.stored, vec![None, None]);
    assert_eq!(matched.stale, vec![id]);
}

#[test]
fn test_match_episodes_keeps_cadences_apart() {
    let sensor = Uuid::new_v4();
    let grab = Uuid::new_v4();
    let matched = match_episodes(
        &[(sensor, "continuous", at(10)), (grab, "spot", at(10))],
        &[("spot", at(10)), ("continuous", at(10))],
    );
    assert_eq!(matched.stored, vec![Some(grab), Some(sensor)]);
    assert!(matched.stale.is_empty());
}

#[test]
fn test_match_episodes_claims_each_stored_row_once() {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let matched = match_episodes(
        &[
            (first, "continuous", at(10)),
            (second, "continuous", at(10)),
        ],
        &[("continuous", at(10))],
    );
    assert_eq!(matched.stored, vec![Some(first)]);
    assert_eq!(matched.stale, vec![second]);
}

#[test]
fn test_match_episodes_empty_inputs() {
    let matched = match_episodes(&[], &[]);
    assert!(matched.stored.is_empty() && matched.stale.is_empty());
}
