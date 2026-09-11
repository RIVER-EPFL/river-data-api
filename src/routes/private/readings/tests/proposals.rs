#[test]
fn only_accept_and_reject_are_decisions() {
    assert!(super::parse_decision("accept").unwrap());
    assert!(!super::parse_decision("reject").unwrap());
    assert!(super::parse_decision("apply").is_err());
    assert!(super::parse_decision("").is_err());
}

/// The reopen rule is decided inside the upsert, so what proves it is the statement: the three
/// decision columns each carry a CASE over the same NULL-safe comparison, and nothing but the
/// entity's own columns is named.
#[test]
fn test_the_proposal_upsert_decides_the_reopen_in_sql() {
    use sea_orm::{ActiveValue, EntityTrait, QueryTrait};
    let row = crate::routes::private::readings::models::change_proposal::ActiveModel {
        id: ActiveValue::Set(uuid::Uuid::nil()),
        stream_id: ActiveValue::Set(uuid::Uuid::nil()),
        time: ActiveValue::Set(chrono::Utc::now()),
        replicate_index: ActiveValue::Set(0),
        proposed_raw_value: ActiveValue::Set(1.0),
        proposed_standard_curve_id: ActiveValue::Set(None),
        stored_raw_value: ActiveValue::Set(2.0),
        stored_standard_curve_id: ActiveValue::Set(None),
        ..Default::default()
    };
    let sql = crate::routes::private::readings::models::change_proposal::Entity::insert(row)
        .on_conflict(super::proposal_conflict())
        .build(sea_orm::DatabaseBackend::Postgres)
        .to_string();
    assert_eq!(
        sql.matches("IS DISTINCT FROM").count(),
        6,
        "two comparisons in each of the three CASEs: {sql}"
    );
    for column in ["status", "decided_by", "decided_at"] {
        assert!(
            sql.contains(&format!("\"{column}\" = (CASE")),
            "{column} is kept or reset by a CASE: {sql}"
        );
    }
    assert!(
        sql.contains("ON CONFLICT (\"stream_id\", \"time\", \"replicate_index\")"),
        "the conflict target is the table's unique key: {sql}"
    );
}
