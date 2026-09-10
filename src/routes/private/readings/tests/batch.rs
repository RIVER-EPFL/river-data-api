use super::{Replace, readings, readings_upsert};
use sea_orm::{EntityTrait, QueryTrait, Set};

/// The `ON CONFLICT` tail of the statement each mode builds.
fn on_conflict_sql(replace: Replace) -> String {
    let model = readings::ActiveModel {
        stream_id: Set(uuid::Uuid::nil()),
        time: Set(chrono::Utc::now().into()),
        replicate_index: Set(0),
        raw_value: Set(1.0),
        ..Default::default()
    };
    let sql = readings::Entity::insert(model)
        .on_conflict(readings_upsert(replace))
        .build(sea_orm::DatabaseBackend::Postgres)
        .to_string();
    sql.split_once("ON CONFLICT")
        .map(|(_, tail)| tail.to_string())
        .unwrap_or_default()
}

/// A curve chosen by hand cannot be recovered by any query, so no upsert may clear it. Every
/// mode either leaves the column alone or preserves the stored one when the incoming row names
/// none.
#[test]
fn no_upsert_clears_a_hand_picked_curve() {
    for replace in [
        Replace::Nothing,
        Replace::Values,
        Replace::ValuesAndAttribution,
    ] {
        let tail = on_conflict_sql(replace);
        let assigns_directly =
            tail.contains(r#""standard_curve_id" = "excluded"."standard_curve_id""#);
        assert!(
            !assigns_directly,
            "{replace:?} would overwrite a hand-picked curve with the incoming row's: {tail}"
        );
    }
}

/// The sample link survives a correction that carries none, or the samples trigger deletes the
/// row's group along with its label and notes.
#[test]
fn a_correction_keeps_the_sample_it_belongs_to() {
    for replace in [Replace::Values, Replace::ValuesAndAttribution] {
        let tail = on_conflict_sql(replace);
        assert!(
            tail.contains("COALESCE") && tail.contains("sample_id"),
            "{replace:?} must preserve the stored sample link: {tail}"
        );
    }
}

/// Arrival time moves only with the value: an unchanged re-assert keeps the stored stamp,
/// a changed value takes a fresh one.
#[test]
fn arrival_stamp_follows_a_value_change() {
    for replace in [Replace::Values, Replace::ValuesAndAttribution] {
        let tail = on_conflict_sql(replace);
        assert!(
            tail.contains("ingested_at") && tail.contains("IS DISTINCT FROM"),
            "{replace:?} must re-stamp ingested_at only on a value change: {tail}"
        );
    }
    let tail = on_conflict_sql(Replace::Nothing);
    assert!(
        !tail.contains("ingested_at"),
        "Replace::Nothing must not touch ingested_at: {tail}"
    );
}

/// Operator state is never a source's to overwrite.
#[test]
fn no_upsert_touches_flag_state() {
    for replace in [
        Replace::Nothing,
        Replace::Values,
        Replace::ValuesAndAttribution,
    ] {
        let tail = on_conflict_sql(replace);
        assert!(
            !tail.contains("is_flagged") && !tail.contains("flag_reason"),
            "{replace:?} must leave flag state alone: {tail}"
        );
    }
}
