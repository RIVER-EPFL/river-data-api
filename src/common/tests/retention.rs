use super::*;

fn defaults() -> Retention {
    Retention {
        job_maintenance: Kept::days(14),
        job_operator: Kept::days(180),
        job_maintenance_max_rows: 50_000,
        sync_events: Kept::days(90),
        ingest_receipts: Kept::days(365),
    }
}

#[test]
fn a_zero_horizon_is_a_disabled_prune_rather_than_an_immediate_one() {
    assert_eq!(Kept::days(0), Kept::Disabled);
    assert_eq!(Kept::days(0).horizon_days(), None);
    assert_eq!(Kept::Forever.horizon_days(), None);
}

#[test]
fn the_curation_record_is_the_one_nothing_prunes() {
    let kept: Vec<_> = defaults()
        .records()
        .into_iter()
        .filter(|r| r.kept == Kept::Forever)
        .map(|r| r.table)
        .collect();
    assert_eq!(kept, vec!["reading_decisions", "replicate_audit_holds"]);
}
