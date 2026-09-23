use super::slope_is_usable;

#[test]
fn test_a_zero_slope_is_refused() {
    assert!(slope_is_usable(Some(0.0)).is_err());
}

#[test]
fn test_any_other_slope_or_none_is_usable() {
    for slope in [Some(1.0), Some(-0.5), Some(1e-9), None] {
        assert!(slope_is_usable(slope).is_ok(), "{slope:?}");
    }
}
