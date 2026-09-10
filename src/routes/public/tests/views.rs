use super::*;

#[test]
fn test_expressed_rounds_a_float32_artefact_to_the_declared_places() {
    // 100.8 stored as a MySQL single-precision float, widened to a double
    assert_eq!(expressed(f64::from(100.8_f32), Some(2)), 100.8);
    assert_eq!(expressed(0.0031, Some(4)), 0.0031);
    assert_eq!(expressed(412.66, Some(1)), 412.7);
    assert_eq!(expressed(-1.005, Some(0)), -1.0);
}

#[test]
fn test_expressed_leaves_an_undeclared_slot_as_stored() {
    let stored = f64::from(100.8_f32);
    assert_eq!(expressed(stored, None).to_bits(), stored.to_bits());
}

#[test]
fn test_express_all_rounds_only_present_cells() {
    let mut cells = vec![Some(f64::from(100.8_f32)), None];
    express_all(&mut cells, Some(2));
    assert_eq!(cells, vec![Some(100.8), None]);
}
