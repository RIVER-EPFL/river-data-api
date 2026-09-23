use sea_orm::sea_query::PostgresQueryBuilder;
use uuid::Uuid;

use crate::routes::private::tools::models::Subject;
use crate::routes::private::tools::service::parameters_of_query;

fn sql(subject: &Subject) -> String {
    parameters_of_query(subject)
        .expect("a statement")
        .to_string(PostgresQueryBuilder)
}

#[test]
fn test_a_parameter_list_needs_no_statement() {
    assert!(parameters_of_query(&Subject::Parameters(vec![Uuid::nil()])).is_none());
}

/// Expected behaviour: a calibration answers with its own parameter, and only one naming none
/// falls back to what its instrument is deployed to measure.
#[test]
fn test_a_calibration_reaches_its_deployments_only_when_it_names_no_parameter() {
    let out = sql(&Subject::Calibration(Uuid::nil()));
    assert!(out.starts_with(r#"SELECT DISTINCT "q"."p""#), "{out}");
    assert!(out.contains("UNION ALL"), "{out}");
    assert!(
        out.contains(r#""c"."sensor_id" = "d"."sensor_id""#),
        "{out}"
    );
    assert!(out.contains(r#""c"."parameter_id" IS NULL"#), "{out}");
    assert!(out.ends_with(r#"WHERE "q"."p" IS NOT NULL"#), "{out}");
}

#[test]
fn test_a_reading_answers_with_its_stream_s_slot_parameter() {
    let out = sql(&Subject::Reading {
        stream_id: Uuid::nil(),
        replicate_index: Some(2),
    });
    assert!(
        out.contains(r#""site_parameters"."id" = "data_streams"."site_parameter_id""#),
        "{out}"
    );
    assert!(!out.contains("replicate_index"), "{out}");
}

/// Expected behaviour: a calculation answers with the catalog parameters its active version's
/// outputs name, matched on code whatever the case.
#[test]
fn test_a_calculation_reads_the_outputs_of_its_active_version() {
    let out = sql(&Subject::Calculation("closure_a".into()));
    assert!(
        out.contains(r#""v"."id" = "s"."active_version_id""#),
        "{out}"
    );
    assert!(
        out.contains(
            r#"jsonb_array_elements(COALESCE("v"."manifest" -> 'outputs', CAST('[]' AS jsonb)))"#
        ),
        "{out}"
    );
    assert!(
        out.contains(r#"LOWER("out"."code") = LOWER("o"."value" ->> 'suggested_parameter_code')"#),
        "{out}"
    );
    assert!(out.contains(r#""s"."name" = 'closure_a'"#), "{out}");
}

/// Expected behaviour: a constant answers with what the active calculations declaring it read,
/// their event inputs and their replicate params, and not with what they write.
#[test]
fn test_a_constant_reads_the_inputs_of_the_calculations_declaring_it() {
    let out = sql(&Subject::Constant(Uuid::nil()));
    assert!(
        out.contains(r#"jsonb_exists(COALESCE("v"."manifest" -> 'constants', CAST('[]' AS jsonb)), "c"."name")"#),
        "{out}"
    );
    assert!(out.contains("INNER JOIN LATERAL"), "{out}");
    assert!(out.contains("'event_inputs'"), "{out}");
    assert!(
        out.contains(r#"("r"."value" ->> 'kind') = 'replicates'"#),
        "{out}"
    );
    assert!(!out.contains("'outputs'"), "{out}");
}
