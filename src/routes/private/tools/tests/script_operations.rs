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

mod activation_arm {
    use crate::routes::private::tools::service::migration_job;
    use uuid::Uuid;

    /// Scenario: an author activates a corrected version of a calculation that has already
    /// produced values at visits.
    #[test]
    fn the_correcting_arm_scopes_the_repair_to_the_version_it_replaced() {
        let superseded = Uuid::new_v4();
        let (params, key) = migration_job(true, "pco2", Some(superseded)).expect("a job");
        assert_eq!(params["version"], serde_json::json!(superseded));
        assert_eq!(params["calculation"], "pco2");
        assert_eq!(key, format!("event_recompute:version:{superseded}"));
    }

    #[test]
    fn the_leaving_arm_repairs_nothing_and_the_history_stands() {
        assert!(migration_job(false, "pco2", Some(Uuid::new_v4())).is_none());
    }

    /// A first activation replaces no version, so there is nothing to migrate however the author
    /// answered.
    #[test]
    fn a_first_activation_supersedes_nothing() {
        assert!(migration_job(true, "pco2", None).is_none());
    }
}
