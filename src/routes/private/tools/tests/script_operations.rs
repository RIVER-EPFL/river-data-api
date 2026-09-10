use super::{check_engine, normalise_name};

#[test]
fn test_a_tool_name_is_lower_cased_and_path_safe() {
    assert_eq!(normalise_name("  DOC  ").expect("trimmed"), "doc");
    assert_eq!(normalise_name("tss_afdm").expect("plain"), "tss_afdm");
    assert!(normalise_name("").is_err());
    assert!(normalise_name("chl a").is_err(), "a space is not a segment");
    assert!(
        normalise_name("co2/air").is_err(),
        "a slash is not a segment"
    );
}

#[test]
fn test_only_the_two_engines_are_accepted() {
    assert!(check_engine("script").is_ok());
    assert!(check_engine("formula").is_ok());
    assert!(check_engine("r").is_err());
}
