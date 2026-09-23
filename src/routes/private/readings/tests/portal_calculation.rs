use uuid::Uuid;

use super::streams_for_columns;

fn carrying(column: &str) -> serde_json::Value {
    serde_json::json!({ "parameter": { "column_name": column } })
}

/// Scenario: a site holds the CNET streams for two of the three columns calcPCO2 reads, plus one
/// it does not read, and one stream whose descriptor names no column.
/// Expected behaviour: each read column held at the site opens its own stream; the third is left
/// out rather than guessed.
#[test]
fn test_each_input_column_opens_the_stream_that_carries_it() {
    let (co2, temp, ph, bare) = (
        Uuid::from_u128(1),
        Uuid::from_u128(2),
        Uuid::from_u128(3),
        Uuid::from_u128(4),
    );
    let candidates = vec![
        (co2, carrying("lab_co2_co2ppm")),
        (temp, carrying("WTW_Temp_degC_1")),
        (ph, carrying("WTW_pH_1")),
        (bare, serde_json::json!({ "parameter": {} })),
    ];
    let inputs = ["lab_co2_co2ppm", "WTW_Temp_degC_1", "Field_BP"].map(String::from);
    let matched = streams_for_columns(&candidates, &inputs);
    assert_eq!(matched.len(), 2);
    assert_eq!(matched["lab_co2_co2ppm"], co2);
    assert_eq!(matched["WTW_Temp_degC_1"], temp);
}

#[test]
fn test_no_candidate_opens_nothing() {
    assert!(streams_for_columns(&[], &["lab_co2_co2ppm".to_string()]).is_empty());
}
