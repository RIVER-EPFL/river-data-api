use super::{GivenUp, unpublished_by};
use uuid::Uuid;

fn given_up(parameter_id: Uuid, calculation: &str) -> GivenUp {
    GivenUp {
        parameter_id,
        calculation: calculation.to_string(),
    }
}

/// Scenario: a formula is ticked as a step, which leaves the parameter it minted in the catalogue
/// with nothing publishing it.
/// Expected behaviour: that parameter reads as the calculation's, by the row it gave up rather
/// than by a code either side may since have been renamed; another parameter reads as an ordinary
/// catalog row.
#[test]
fn test_a_parameter_a_formula_gave_up_names_the_calculation() {
    let k1 = Uuid::new_v4();
    let gave_up = vec![given_up(k1, "Dissolved organic carbon")];
    assert_eq!(
        unpublished_by(k1, &gave_up).as_deref(),
        Some("Dissolved organic carbon")
    );
    assert_eq!(unpublished_by(Uuid::new_v4(), &gave_up), None);
}

#[test]
fn test_nothing_given_up_leaves_every_parameter_ordinary() {
    assert_eq!(unpublished_by(Uuid::new_v4(), &[]), None);
}
