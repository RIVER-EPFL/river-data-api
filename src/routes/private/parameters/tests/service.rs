use super::{GivenUp, Publisher, curve_collisions, decommissioned_by, unpublished_by};
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

fn publisher(parameter_id: Uuid, calculation: &str, decommissioned_at: Option<&str>) -> Publisher {
    Publisher {
        parameter_id,
        tool_script_id: Uuid::new_v4(),
        calculation: calculation.to_string(),
        decommissioned_at: decommissioned_at.map(|at| at.parse().unwrap()),
    }
}

/// Scenario: the calculation publishing pCO2 is decommissioned.
/// Expected behaviour: the parameter reads as decommissioned by it, with the decommission's date.
#[test]
fn test_a_parameter_only_a_decommissioned_calculation_publishes_names_it() {
    let pco2 = Uuid::new_v4();
    let publishers = vec![publisher(pco2, "pco2", Some("2026-09-24T10:00:00Z"))];
    let found = decommissioned_by(pco2, &publishers).expect("decommissioned");
    assert_eq!(found.calculation, "pco2");
    assert_eq!(found.at.to_rfc3339(), "2026-09-24T10:00:00+00:00");
    assert!(decommissioned_by(Uuid::new_v4(), &publishers).is_none());
}

/// Scenario: a live calculation took the parameter over, or the calculation was recommissioned.
/// Expected behaviour: a live publisher is what computes it, so it is not decommissioned.
#[test]
fn test_a_live_publisher_clears_the_decommission() {
    let pco2 = Uuid::new_v4();
    let publishers = vec![
        publisher(pco2, "pco2", Some("2026-09-24T10:00:00Z")),
        publisher(pco2, "pco2 revised", None),
    ];
    assert!(decommissioned_by(pco2, &publishers).is_none());
}

/// Expected behaviour: of two decommissioned publishers, the latest decommission names it.
#[test]
fn test_the_latest_decommission_names_the_parameter() {
    let pco2 = Uuid::new_v4();
    let publishers = vec![
        publisher(pco2, "evan", Some("2026-09-20T10:00:00Z")),
        publisher(pco2, "evan2", Some("2026-09-24T10:00:00Z")),
    ];
    assert_eq!(
        decommissioned_by(pco2, &publishers).unwrap().calculation,
        "evan2"
    );
}

/// Expected behaviour: a curve collides only where the same instrument already opens a curve at
/// the same instant on the channel it would land on.
#[test]
fn test_curve_collisions_are_same_instrument_same_instant() {
    let (x, y) = (Uuid::from_u128(1), Uuid::from_u128(2));
    let may: chrono::DateTime<chrono::Utc> = "2025-05-01T00:00:00Z".parse().unwrap();
    let june: chrono::DateTime<chrono::Utc> = "2025-06-01T00:00:00Z".parse().unwrap();
    assert_eq!(
        curve_collisions(&[(x, may), (x, june), (y, may)], &[(x, may), (y, june)]),
        vec![(x, may)]
    );
    assert!(curve_collisions(&[(x, may)], &[]).is_empty());
}
