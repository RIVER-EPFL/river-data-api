use super::*;
use crate::routes::private::sync::models::{HoldKind, HoldStatus};
use sea_orm::{EntityTrait, QueryFilter, QueryTrait};

fn sql(condition: sea_orm::Condition) -> String {
    Entity::find()
        .filter(condition)
        .build(sea_orm::DatabaseBackend::Postgres)
        .to_string()
}

fn at() -> chrono::DateTime<chrono::Utc> {
    "2026-06-19T12:00:00Z".parse().unwrap()
}

/// A finding the audit or the chain raised carries no stream: the slot is the whole key.
#[test]
fn test_a_slot_is_keyed_on_site_parameter_and_instant_with_no_stream() {
    let out = sql(slot(Uuid::nil(), Uuid::nil(), at()));
    assert!(out.contains(r#""stream_id" IS NULL"#), "{out}");
    assert!(
        out.contains(r#""site_id" = '00000000-0000-0000-0000-000000000000'"#),
        "{out}"
    );
    assert!(
        out.contains(r#""parameter_id" = '00000000-0000-0000-0000-000000000000'"#),
        "{out}"
    );
    assert!(
        out.contains(r#""group_time" = '2026-06-19 12:00:00"#),
        "{out}"
    );
}

/// The status and the kinds are bound as values, not pasted into the statement, so a renamed
/// variant is a compile error rather than a supersede that matches no row.
#[test]
fn test_status_and_kinds_are_bound_from_their_own_vocabularies() {
    let out = sql(of_kinds(
        in_status(slot(Uuid::nil(), Uuid::nil(), at()), HoldStatus::Pending),
        &[HoldKind::MissingOutput, HoldKind::StaleOutput],
    ));
    assert!(out.contains(r#""status" = 'pending'"#), "{out}");
    assert!(
        out.contains(r#""kind" IN ('missing_output', 'stale_output')"#),
        "{out}"
    );
}

/// The probe and the two supersedes differ only in what they narrow the slot by, which is what
/// makes one of them closing a finding the other cannot see impossible.
#[test]
fn test_narrowing_only_adds_to_the_slot_predicate() {
    let narrowed = sql(in_status(
        slot(Uuid::nil(), Uuid::nil(), at()),
        HoldStatus::Pending,
    ));
    for clause in [
        r#""stream_id" IS NULL"#,
        r#""site_id" = '00000000-0000-0000-0000-000000000000'"#,
        r#""parameter_id" = '00000000-0000-0000-0000-000000000000'"#,
        r#""group_time" = '2026-06-19 12:00:00"#,
    ] {
        assert!(
            narrowed.contains(clause),
            "{clause} missing from {narrowed}"
        );
    }
}

/// A stream-keyed hold stands at its stream's pairing, a stream-less one at the slot it names.
#[test]
fn test_with_stream_slot_reads_the_hold_first_then_the_pairing() {
    let out = with_stream_slot()
        .expr(slot_site())
        .expr(slot_parameter())
        .to_owned()
        .to_string(sea_orm::sea_query::PostgresQueryBuilder);
    assert!(
        out.contains(r#"LEFT JOIN "data_streams" AS "ds" ON "ds"."id" = "h"."stream_id""#),
        "{out}"
    );
    assert!(
        out.contains(
            r#"LEFT JOIN "site_parameters" AS "sp" ON "sp"."id" = "ds"."site_parameter_id""#
        ),
        "{out}"
    );
    assert!(
        out.contains(r#"COALESCE("h"."site_id", "sp"."site_id")"#),
        "{out}"
    );
    assert!(
        out.contains(r#"COALESCE("h"."parameter_id", "sp"."parameter_id")"#),
        "{out}"
    );
}
