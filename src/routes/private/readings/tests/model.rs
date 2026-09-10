use sea_orm::ActiveValue::{NotSet, Set};

/// The arrival stamp is the database's, and a fresh row carries no curation, no event and no
/// origin blob. Every write path builds on these, so they are stated once.
#[test]
fn test_a_new_reading_is_uncurated_and_unstamped() {
    let model = super::new(uuid::Uuid::nil(), chrono::Utc::now().into(), 2, 7.5);
    assert!(matches!(model.ingested_at, NotSet));
    assert_eq!(model.replicate_index, Set(2));
    assert_eq!(model.raw_value, Set(7.5));
    assert_eq!(model.is_flagged, Set(Some(false)));
    assert_eq!(model.flag_reason, Set(None));
    assert_eq!(model.withdrawn_at, Set(None));
    assert_eq!(model.withdrawn_reason, Set(None));
    assert_eq!(model.collection_event_id, Set(None));
    assert_eq!(model.sample_id, Set(None));
    assert_eq!(model.provenance, Set(None));
    assert_eq!(model.provenance_kind, Set(None));
    assert_eq!(model.label, Set(None));
    assert_eq!(model.notes, Set(None));
    assert_eq!(model.created_by, Set(None));
    assert_eq!(model.calibrated_value, Set(None));
    assert_eq!(model.standard_curve_id, Set(None));
}
