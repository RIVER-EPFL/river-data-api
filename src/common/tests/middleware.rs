use super::{Direction, crud_entity, crud_scope_condition};
use uuid::Uuid;

#[test]
fn test_crud_entity_reads_the_entity_and_ignores_what_follows_it() {
    assert_eq!(crud_entity("/api/site_parameters"), Some("site_parameters"));
    assert_eq!(
        crud_entity("/api/site_parameters/batch"),
        Some("site_parameters"),
        "`batch` is a sub-route of the entity, not another entity"
    );
    assert_eq!(
        crud_entity("/api/site_parameters/0189d3f0-0000-4000-8000-000000000000"),
        Some("site_parameters")
    );
    assert_eq!(crud_entity("/"), None);
}

/// An entity answering `None` has no project dimension, which is what makes
/// `inject_project_scope` refuse a scoped token's write to it. The refusal is the one rule no
/// row filter can state, so the set it applies to is asserted here.
#[test]
fn test_the_entities_with_no_project_dimension_are_the_ones_a_scoped_token_is_refused() {
    let projects = [Uuid::nil()];
    let none_for = |direction| {
        let mut names: Vec<&str> = Vec::new();
        for entity in [
            "projects",
            "sites",
            "site_parameters",
            "notes",
            "annotations",
            "samples",
            "subprojects",
            "data_streams",
            "reading_decisions",
            "replicate_audit_holds",
            "alarm_thresholds",
            "alarm_events",
            "sensor_deployments",
            "sensor_calibrations",
            "standard_curves",
            "sensors",
            "reprocessing_jobs",
            "reprocessing_job_logs",
            "parameters",
            "constants",
            "schedules",
            "collection_events",
            "tool_runs",
            "ingest_receipts",
            "change_audit_entries",
            "notification_mutes",
            "meteoswiss_subscriptions",
        ] {
            if crud_scope_condition(entity, &projects, direction).is_none() {
                names.push(entity);
            }
        }
        names
    };
    assert_eq!(
        none_for(Direction::Read),
        ["parameters", "constants", "schedules"],
        "a read is confined for every entity whose rows carry a project"
    );
    let member = vec![
        "projects",
        "sensors",
        "reprocessing_jobs",
        "parameters",
        "constants",
        "schedules",
    ];
    let mut token = member.clone();
    token.push("meteoswiss_subscriptions");
    for (direction, mut want) in [
        (Direction::MemberWrite, member),
        (Direction::TokenWrite, token),
    ] {
        let mut got = none_for(direction);
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(
            got, want,
            "the shared inventory is written as catalog, not as a project's"
        );
    }
}

/// An audit row is read where the row it records is: a site-bearing subject by its site, a
/// catalog subject by anyone, and a subject of any other kind by nobody confined.
#[test]
fn test_change_audit_confines_site_bearing_subjects_and_passes_catalog_ones() {
    let sql = format!(
        "{:?}",
        crud_scope_condition("change_audit_entries", &[Uuid::nil()], Direction::Read)
            .expect("change_audit_entries is project-bound")
    );
    for prefix in [
        "site:",
        "site_parameter:",
        "sensor_calibration:",
        "standard_curve:",
    ] {
        assert!(
            sql.contains(prefix),
            "{prefix} subjects are confined by their site: {sql}"
        );
    }
    for prefix in [
        "parameter:",
        "constant:",
        "calculation_formula:",
        "derived_parameter_source:",
        "parameter_group:",
        "schedule:",
    ] {
        assert!(sql.contains(prefix), "{prefix} subjects are catalog: {sql}");
    }
    assert!(
        !sql.contains("push_subscriptions"),
        "a person's device trail is no project's"
    );
}

/// The bench instrument is the one row a member may write and may not read, so the asymmetry is
/// asserted rather than described.
#[test]
fn test_only_a_members_write_reaches_a_curve_on_an_instrument_deployed_nowhere() {
    let projects = [Uuid::nil()];
    let sql = |entity: &str, direction| {
        format!(
            "{:?}",
            crud_scope_condition(entity, &projects, direction).expect("a project-bound entity")
        )
    };
    assert_ne!(
        sql("standard_curves", Direction::Read),
        sql("standard_curves", Direction::MemberWrite),
        "a member's write must reach a curve on an instrument deployed nowhere"
    );
    assert_eq!(
        sql("standard_curves", Direction::Read),
        sql("standard_curves", Direction::TokenWrite),
        "a project-scoped token is confined to where the instrument stood, in both directions"
    );
    for entity in [
        "sites",
        "notes",
        "sensor_calibrations",
        "data_streams",
        "reading_decisions",
    ] {
        assert_eq!(
            sql(entity, Direction::Read),
            sql(entity, Direction::MemberWrite),
            "{entity} has one rule in every direction"
        );
    }
}
