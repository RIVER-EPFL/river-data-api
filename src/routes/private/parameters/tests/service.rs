use super::{GivenUp, unpublished_by};

fn given_up(code: &str, calculation: &str) -> GivenUp {
    GivenUp {
        code: code.to_string(),
        calculation: calculation.to_string(),
    }
}

/// Scenario: a formula is ticked as a step, which leaves the parameter it minted in the catalogue
/// with nothing publishing it.
/// Expected behaviour: that parameter reads as the calculation's, by whatever case its code is
/// written in; a parameter no formula gave up reads as an ordinary catalog row.
#[test]
fn test_a_parameter_a_formula_gave_up_names_the_calculation() {
    let gave_up = vec![given_up("k1", "Dissolved organic carbon")];
    assert_eq!(
        unpublished_by("k1", &gave_up).as_deref(),
        Some("Dissolved organic carbon")
    );
    assert_eq!(
        unpublished_by(" K1 ", &gave_up).as_deref(),
        Some("Dissolved organic carbon")
    );
    assert_eq!(unpublished_by("doc", &gave_up), None);
}

#[test]
fn test_nothing_given_up_leaves_every_parameter_ordinary() {
    assert_eq!(unpublished_by("k1", &[]), None);
}
