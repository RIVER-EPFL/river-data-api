use super::*;

#[test]
fn an_unknown_estimator_is_refused_rather_than_defaulted() {
    assert!(parse("populaton").is_err());
    assert!(parse("").is_err());
    assert_eq!(parse(POPULATION).unwrap(), POPULATION);
    assert_eq!(parse_opt(None).unwrap(), None);
}

#[test]
fn the_fallback_is_a_sample_sd_that_reads_as_undeclared() {
    let r = Resolved::undeclared();
    assert_eq!(r.estimator, SAMPLE);
    assert_eq!(r.source.as_str(), "default");
    assert!(!r.is_declared());
}
