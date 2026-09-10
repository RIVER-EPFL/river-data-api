use super::value_source;

/// Expected behaviour: a spot period is summarised at the served instant value, a continuous one
/// at the reading. Reading the wrong side would summarise replicates as if each were a
/// measurement of its own.
#[test]
fn each_cadence_is_summarised_over_what_the_api_serves_for_it() {
    let spot = value_source("spot");
    assert!(spot.contains("smp.mean"), "the spot arm reads the mean");
    assert!(
        spot.contains("withdrawn_at IS NULL"),
        "a retracted replicate is not in the period"
    );

    let continuous = value_source("continuous");
    assert!(
        continuous.contains("r.replicate_index = 0"),
        "continuous rows live at index 0"
    );
    assert!(
        !continuous.contains("samples"),
        "a continuous reading has no sample to average"
    );
}
