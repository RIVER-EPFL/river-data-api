use sea_orm::QueryTrait;

use super::*;

fn occupant(sensor: Uuid, until: Option<DateTime<Utc>>) -> SlotOccupant {
    SlotOccupant {
        deployment_id: Uuid::nil(),
        sensor_id: sensor,
        deployed_from: DateTime::from_timestamp(0, 0).expect("epoch"),
        deployed_until: until,
    }
}

#[test]
fn a_self_collision_reads_as_an_edit_not_a_recall() {
    let sensor = Uuid::from_u128(1);
    let message = conflict_message(&occupant(sensor, None), sensor, "deploy");
    assert!(
        message.contains("Edit that deployment instead"),
        "recalling itself is not a remedy the operator can act on: {message}"
    );
}

#[test]
fn another_instruments_collision_keeps_the_recall_wording() {
    let message = conflict_message(
        &occupant(Uuid::from_u128(1), None),
        Uuid::from_u128(2),
        "deploy",
    );
    assert!(
        message.starts_with("Another sensor is already deployed to this site for this parameter"),
        "{message}"
    );
    assert!(
        message.contains("Recall it first, then deploy"),
        "{message}"
    );
}

#[test]
fn an_open_window_is_named_as_open() {
    let message = conflict_message(
        &occupant(Uuid::from_u128(1), None),
        Uuid::from_u128(2),
        "deploy",
    );
    assert!(message.contains("to open"), "{message}");
}

#[test]
fn only_the_exclusion_constraint_reads_as_a_slot_conflict() {
    assert!(is_slot_conflict(&DbErr::Custom(
        "conflicting key value violates exclusion constraint \
         \"excl_deployment_site_param_slot\""
            .to_string()
    )));
    assert!(is_slot_conflict(&DbErr::Custom(
        "SQLSTATE 23P01".to_string()
    )));
    assert!(!is_slot_conflict(&DbErr::Custom(
        "duplicate key value violates unique constraint \"sensors_pkey\"".to_string()
    )));
}

/// The open deployment first, else the most recent: the rule that decides which slot a reprocess
/// rewrites when its job names no parameter.
#[test]
fn a_reprocess_prefers_the_open_deployment_then_the_most_recent() {
    let sql = current_deployment(Uuid::from_u128(1), Uuid::from_u128(2))
        .build(sea_orm::DatabaseBackend::Postgres)
        .to_string();
    assert!(
        sql.contains(
            "ORDER BY \"deployed_until\" IS NULL DESC, \"sensor_deployments\".\"deployed_from\" DESC"
        ),
        "{sql}"
    );
    assert!(sql.contains("\"sensor_id\" = "), "{sql}");
    assert!(sql.contains("\"site_id\" = "), "{sql}");
}
