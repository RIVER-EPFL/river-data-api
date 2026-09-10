use super::{InstrumentKind, source_instrument_name};

#[test]
fn test_source_instrument_name_source_parameter_carries_no_site() {
    let name = source_instrument_name(
        InstrumentKind::SourceParameter,
        "DOC_avg_ppb",
        "cnet",
        Some("FP1 DOC_avg_ppb"),
    );
    assert_eq!(name, "DOC_avg_ppb (cnet)");
}

#[test]
fn test_source_instrument_name_lab_carries_no_site() {
    let name = source_instrument_name(InstrumentKind::Lab, "DOC", "cnet", Some("FP1 DOC"));
    assert_eq!(name, "DOC (cnet)");
}

#[test]
fn test_source_instrument_name_entry_channel_keeps_slot_name() {
    let name = source_instrument_name(
        InstrumentKind::EntryChannel,
        "Depth",
        "grab_sample",
        Some("Martigny Depth (grab_sample)"),
    );
    assert_eq!(name, "Martigny Depth (grab_sample)");
}

#[test]
fn test_source_instrument_name_falls_back_without_a_hint() {
    let name = source_instrument_name(InstrumentKind::EntryChannel, "Depth", "api", None);
    assert_eq!(name, "Depth (api)");
}

use super::*;

#[test]
fn claims_a_free_serial() {
    assert_eq!(
        serial_to_claim(Some("919402"), None),
        Some("919402".to_string())
    );
}

#[test]
fn leaves_a_held_serial_alone() {
    assert_eq!(serial_to_claim(Some("919402"), Some(Uuid::nil())), None);
}

#[test]
fn treats_an_absent_or_blank_serial_as_none() {
    assert_eq!(serial_to_claim(None, None), None);
    assert_eq!(serial_to_claim(Some("   "), None), None);
}

#[test]
fn trims_the_claimed_serial() {
    assert_eq!(
        serial_to_claim(Some(" 4000138 "), None),
        Some("4000138".to_string())
    );
}
