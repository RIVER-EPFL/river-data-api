use super::*;

/// Scenario: the pairing backfill's two attributions are built rather than written out.
/// Expected behaviour: each writes only what the slot decides, leaves an already attributed row
/// alone, and is scoped to the stream or the plan the caller named.
#[test]
fn test_attribute_readings_fills_only_unattributed_rows() {
    let sql =
        attribute_readings(HoldScope::Stream(Uuid::nil()), None).to_string(PostgresQueryBuilder);

    assert!(sql.starts_with(r#"UPDATE "readings""#), "{sql}");
    assert!(
        sql.contains(r#""site_id" = "site_parameters"."site_id""#),
        "{sql}"
    );
    assert!(
        sql.contains(r#"COALESCE("data_streams"."sensor_id", "readings"."sensor_id")"#),
        "{sql}"
    );
    assert!(
        sql.contains(
            r#"COALESCE("readings"."measurement_type", "data_streams"."measurement_type")"#
        ),
        "the stream's cadence is the fallback, not the override: {sql}"
    );
    assert!(
        sql.contains(r#""readings"."site_id" IS NULL"#),
        "an attributed reading keeps what it has: {sql}"
    );
    assert!(
        sql.contains(r#""data_streams"."site_parameter_id" = "site_parameters"."id""#),
        "{sql}"
    );
    assert!(sql.contains(r#""data_streams"."id" ="#), "{sql}");
}

#[test]
fn test_attribute_scopes_to_the_stream_or_the_plan() {
    let by_stream =
        attribute_readings(HoldScope::Stream(Uuid::nil()), None).to_string(PostgresQueryBuilder);
    assert!(
        by_stream.contains(r#""data_streams"."id" ="#),
        "{by_stream}"
    );

    let by_plan =
        attribute_readings(HoldScope::Plan(Uuid::nil()), None).to_string(PostgresQueryBuilder);
    assert!(
        by_plan.contains(r#""data_streams"."pairing_plan_id" ="#),
        "{by_plan}"
    );
}

/// The non-numeric series carries no value, so its attribution writes the slot and the instrument
/// and nothing else.
#[test]
fn test_attribute_status_events_writes_the_slot_and_the_instrument() {
    let sql =
        attribute_status_events(HoldScope::Stream(Uuid::nil())).to_string(PostgresQueryBuilder);

    assert!(sql.starts_with(r#"UPDATE "status_events""#), "{sql}");
    assert!(!sql.contains("measurement_type"), "{sql}");
    assert!(!sql.contains("deployment_id"), "{sql}");
    assert!(
        sql.contains(r#""status_events"."site_id" IS NULL"#),
        "{sql}"
    );
}

#[test]
fn test_forget_window_digests_clears_only_the_scope() {
    let sql = forget_window_digests(HoldScope::Plan(Uuid::nil())).to_string(PostgresQueryBuilder);

    assert!(sql.starts_with(r#"UPDATE "data_streams""#), "{sql}");
    assert!(sql.contains(r#""last_window_digest" = NULL"#), "{sql}");
    assert!(
        sql.contains(r#""data_streams"."pairing_plan_id" ="#),
        "{sql}"
    );
}
