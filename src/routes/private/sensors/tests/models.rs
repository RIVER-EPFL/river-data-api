use super::InstrumentKind;

/// `kind` is the fact and `is_lab_instrument` is its shadow: the column decides, and the flag
/// is read only where no kind was stored.
#[test]
fn the_stored_kind_decides_and_the_flag_is_only_the_fallback() {
    for (stored, want) in [
        ("device", InstrumentKind::Device),
        ("lab", InstrumentKind::Lab),
        ("source_parameter", InstrumentKind::SourceParameter),
        ("entry_channel", InstrumentKind::EntryChannel),
    ] {
        for flag in [None, Some(false), Some(true)] {
            assert_eq!(
                InstrumentKind::of(Some(stored), flag),
                want,
                "{stored} with flag {flag:?}"
            );
        }
    }
}

#[test]
fn a_row_predating_the_backfill_falls_back_to_the_flag() {
    assert_eq!(InstrumentKind::of(None, Some(true)), InstrumentKind::Lab);
    assert_eq!(
        InstrumentKind::of(None, Some(false)),
        InstrumentKind::Device
    );
    assert_eq!(InstrumentKind::of(None, None), InstrumentKind::Device);
    assert_eq!(
        InstrumentKind::of(Some(""), Some(true)),
        InstrumentKind::Lab
    );
}

/// The collapse this replaces: to the flag alone, all three non-device kinds read as lab.
#[test]
fn only_lab_is_lab_where_the_kind_is_stored() {
    let stored = ["lab", "source_parameter", "entry_channel"];
    let lab: Vec<bool> = stored
        .iter()
        .map(|k| InstrumentKind::of(Some(k), Some(true)) == InstrumentKind::Lab)
        .collect();
    assert_eq!(lab, vec![true, false, false]);
    for k in stored {
        assert!(InstrumentKind::of(Some(k), Some(true)).is_lab_instrument());
    }
}

#[test]
fn the_two_bookkeeping_kinds_are_the_ones_nothing_measured_on() {
    assert!(InstrumentKind::SourceParameter.is_bookkeeping());
    assert!(InstrumentKind::EntryChannel.is_bookkeeping());
    assert!(!InstrumentKind::Device.is_bookkeeping());
    assert!(!InstrumentKind::Lab.is_bookkeeping());
}
