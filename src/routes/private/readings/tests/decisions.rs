use super::{Kind, Selection, SelectionKey, keyed_corrections, old_state_for};

#[test]
fn every_kind_round_trips_its_name() {
    for k in [
        Kind::Flag,
        Kind::Unflag,
        Kind::Withdraw,
        Kind::Reassert,
        Kind::Curve,
        Kind::CalibrationPin,
        Kind::InstrumentPin,
        Kind::SlotMove,
        Kind::ValueCorrection,
        Kind::UnverifiedEntry,
        Kind::Verify,
        Kind::Reject,
        Kind::Chain,
        Kind::Detach,
        Kind::Return,
        Kind::Retag,
        Kind::Attribution,
        Kind::Rollback,
    ] {
        assert_eq!(Kind::parse(k.as_str()), Some(k));
    }
    assert_eq!(Kind::parse("delete"), None);
}

#[test]
fn test_recomposes_a_value_or_curve_edit_and_nothing_else() {
    assert!(Kind::ValueCorrection.recomposes());
    assert!(Kind::Curve.recomposes());
    for k in [
        Kind::Flag,
        Kind::Withdraw,
        Kind::Verify,
        Kind::CurveRetire,
        Kind::Rollback,
    ] {
        assert!(
            !k.recomposes(),
            "{} moves no curve and no raw value",
            k.as_str()
        );
    }
}

#[test]
fn each_kind_projects_to_its_own_columns_and_ownership_kinds_project_nothing() {
    assert_eq!(
        Kind::Flag.projected_columns(),
        ["is_flagged", "flag_reason"]
    );
    assert_eq!(
        Kind::Unflag.projected_columns(),
        ["is_flagged", "flag_reason"]
    );
    assert_eq!(
        Kind::Withdraw.projected_columns(),
        ["withdrawn_at", "withdrawn_reason"]
    );
    assert_eq!(
        Kind::Reject.projected_columns(),
        ["withdrawn_at", "withdrawn_reason", "unverified"]
    );
    assert_eq!(Kind::Curve.projected_columns(), ["standard_curve_id"]);
    assert_eq!(Kind::InstrumentPin.projected_columns(), ["sensor_id"]);
    assert_eq!(Kind::CalibrationPin.projected_columns(), ["calibration_id"]);
    assert_eq!(
        Kind::ValueCorrection.projected_columns(),
        ["raw_value"],
        "the corrected value is recomposed from the row's own curves, and the arrival stamp is \
         the row's first, which no decision moves"
    );
    assert_eq!(Kind::Verify.projected_columns(), ["unverified"]);
    for k in [
        Kind::Chain,
        Kind::Detach,
        Kind::Return,
        Kind::SlotMove,
        Kind::Rollback,
    ] {
        assert!(k.projected_columns().is_empty(), "{k:?}");
    }
}

/// The reader asks the API which decisions it may undo rather than keeping its own copy of the
/// list, so this is the one place the two can disagree.
#[test]
fn reversible_is_exactly_the_kinds_rollback_accepts() {
    for k in [
        Kind::Flag,
        Kind::Unflag,
        Kind::Withdraw,
        Kind::Reassert,
        Kind::Reject,
        Kind::Curve,
        Kind::CalibrationPin,
        Kind::InstrumentPin,
        Kind::ValueCorrection,
        Kind::UnverifiedEntry,
        Kind::Verify,
    ] {
        assert!(k.reversible(), "{k:?} restores the columns it projected");
    }
    for k in [
        Kind::SlotMove,
        Kind::Chain,
        Kind::Detach,
        Kind::Return,
        Kind::Rollback,
    ] {
        assert!(!k.reversible(), "{k:?} projects nothing to restore");
    }
}

#[test]
fn a_family_pairs_a_decision_with_what_undoes_it() {
    assert_eq!(Kind::Flag.family(), Kind::Unflag.family());
    assert_eq!(Kind::Withdraw.family(), Kind::Reassert.family());
    assert_eq!(Kind::Withdraw.family(), Kind::Reject.family());
    assert_eq!(Kind::UnverifiedEntry.family(), Kind::Verify.family());
    assert_ne!(Kind::Flag.family(), Kind::Withdraw.family());
    assert_ne!(Kind::InstrumentPin.family(), Kind::CalibrationPin.family());
    assert_eq!(Kind::Rollback.family(), None);
}

#[test]
fn old_state_keeps_only_the_columns_the_kind_touches() {
    let state = serde_json::json!({
        "is_flagged": true, "flag_reason": "x", "raw_value": 1.5,
        "calibrated_value": 1.7, "ingested_at": "2025-06-15T10:00:00Z",
        "unverified": false, "sensor_id": null
    });
    assert_eq!(
        old_state_for(Kind::Flag, &state),
        serde_json::json!({ "is_flagged": true, "flag_reason": "x" })
    );
    assert_eq!(
        old_state_for(Kind::ValueCorrection, &state),
        serde_json::json!({ "raw_value": 1.5 })
    );
    // An ownership decision records the value and the run it supersedes, and projects nothing.
    assert_eq!(
        old_state_for(Kind::Chain, &state),
        serde_json::json!({ "raw_value": 1.5, "run_id": null })
    );
    // An absent column is recorded as null, so a rollback clears it rather than skipping it.
    assert_eq!(
        old_state_for(Kind::Curve, &state),
        serde_json::json!({ "standard_curve_id": null })
    );
}

/// The rule Q117 settled, named where it can fail: attribution is corrected on the record that
/// decides it, so no writer records a pin. The kinds stay in the vocabulary because stored rows
/// carry them, which is what makes an accidental re-mint possible and this guard necessary.
#[test]
fn no_writer_records_an_attribution_pin() {
    use super::Writer;
    for kind in [Kind::CalibrationPin, Kind::InstrumentPin] {
        assert!(!kind.writable(), "{kind:?}");
        assert!(super::refuse_historical(kind).is_err(), "{kind:?}");
    }
    // Every other kind is still recordable, so the guard is a rule and not a blanket refusal.
    for kind in [
        Kind::Flag,
        Kind::Curve,
        Kind::ValueCorrection,
        Kind::CurveRetire,
        Kind::FormulaTransition,
    ] {
        assert!(kind.writable(), "{kind:?}");
        assert!(super::refuse_historical(kind).is_ok(), "{kind:?}");
    }
    // No writer declares one either: the registry is the other half of the same rule.
    for w in [
        Writer::FlagRoute,
        Writer::GrabReplace,
        Writer::CurveRetirement,
        Writer::DerivedRecompute,
    ] {
        let recorded = w.decision().map(|(k, _)| k);
        assert!(
            recorded != Some(Kind::CalibrationPin) && recorded != Some(Kind::InstrumentPin),
            "{w:?} records {recorded:?}"
        );
    }
}

#[test]
fn every_writer_is_classified_and_derivation_writers_append_nothing() {
    use super::{Origin, Writer};
    let curation = [
        (Writer::FlagRoute, Kind::Flag, Origin::Manual),
        (Writer::UnflagRoute, Kind::Unflag, Origin::Manual),
        (Writer::AuditResolveFlag, Kind::Flag, Origin::Audit),
        (Writer::AuditReopen, Kind::Unflag, Origin::Audit),
        (Writer::WindowedWithdraw, Kind::Withdraw, Origin::Sync),
        (Writer::WindowedReassert, Kind::Reassert, Origin::Sync),
        (Writer::CsvDisplacement, Kind::Withdraw, Origin::Csv),
        (Writer::CsvDisplacementReversal, Kind::Reassert, Origin::Csv),
        (Writer::GrabCurveClaim, Kind::Curve, Origin::Manual),
        (Writer::IngestCurveClaim, Kind::Curve, Origin::Sync),
        (Writer::GrabReplace, Kind::ValueCorrection, Origin::Manual),
        (Writer::IngestOverwrite, Kind::ValueCorrection, Origin::Sync),
        (
            Writer::BatchOverwrite,
            Kind::ValueCorrection,
            Origin::Manual,
        ),
        (Writer::ChainSave, Kind::Chain, Origin::Chain),
        (Writer::MergeMove, Kind::SlotMove, Origin::Manual),
        (
            Writer::DerivedRecompute,
            Kind::FormulaTransition,
            Origin::System,
        ),
        (
            Writer::JanitorRecompose,
            Kind::CurveRecompose,
            Origin::Janitor,
        ),
        (
            Writer::DerivedGapFill,
            Kind::DerivedComputed,
            Origin::System,
        ),
        (Writer::ReprocessSensor, Kind::Reprocess, Origin::System),
        (Writer::ReprocessSlot, Kind::Reprocess, Origin::System),
        (Writer::BackfillAttribution, Kind::Reprocess, Origin::System),
        (Writer::MeasurementRetag, Kind::Retag, Origin::System),
        (Writer::PairingBackfill, Kind::Attribution, Origin::System),
        (Writer::AdoptClaim, Kind::Attribution, Origin::System),
        (
            Writer::DeploymentRollback,
            Kind::Attribution,
            Origin::System,
        ),
        (Writer::SlotRelease, Kind::Attribution, Origin::System),
    ];
    for (w, k, o) in curation {
        assert_eq!(w.decision(), Some((k, o)), "{w:?}");
    }
    // The resolver stamps a reading's first state as it arrives: nothing on the row moved.
    assert_eq!(Writer::CalibrationResolver.decision(), None);
}

#[test]
fn the_kinds_that_move_a_served_value_fire_the_recompute() {
    for k in [
        Kind::Flag,
        Kind::Unflag,
        Kind::Withdraw,
        Kind::Reassert,
        Kind::SlotMove,
        Kind::ValueCorrection,
    ] {
        assert!(k.fires_recompute(), "{k:?}");
    }
    for k in [
        Kind::Curve,
        Kind::InstrumentPin,
        Kind::Chain,
        Kind::Detach,
        Kind::Verify,
    ] {
        assert!(!k.fires_recompute(), "{k:?}");
    }
}

#[test]
fn a_pin_exclusion_names_the_kind_and_covers_group_pins() {
    let rendered = |alias, kind| {
        sea_orm::sea_query::Query::select()
            .expr(sea_orm::sea_query::Expr::val(1))
            .and_where(super::not_pinned(alias, kind))
            .to_string(sea_orm::sea_query::PostgresQueryBuilder)
    };
    let sql = rendered("r", Kind::InstrumentPin);
    assert!(sql.contains(r#""d"."kind" = 'instrument_pin'"#));
    assert!(sql.contains(r#""d"."rolled_back_by" IS NULL"#));
    assert!(sql.contains(
        r#""d"."replicate_index" IS NULL OR "d"."replicate_index" = "r"."replicate_index""#
    ));
    assert!(rendered("tgt", Kind::CalibrationPin).contains(r#""tgt"."stream_id""#));
}

#[test]
fn a_selection_names_a_stream_a_slot_or_keys_and_nothing_else() {
    use super::{Selection, SelectionKey};
    let sql = |selection: &Selection| {
        sea_orm::sea_query::Query::select()
            .expr(sea_orm::sea_query::Expr::val(1))
            .cond_where(selection.condition().unwrap())
            .to_string(sea_orm::sea_query::PostgresQueryBuilder)
    };
    let none = Selection::default();
    assert!(none.condition().is_err(), "nothing selected is refused");
    let by_stream = Selection {
        stream_id: Some(uuid::Uuid::nil()),
        ..Default::default()
    };
    assert!(
        sql(&by_stream)
            .ends_with(r#"WHERE "r"."stream_id" = '00000000-0000-0000-0000-000000000000'"#)
    );
    let half_slot = Selection {
        site_id: Some(uuid::Uuid::nil()),
        ..Default::default()
    };
    assert!(half_slot.condition().is_err(), "a slot needs both ids");
    let at = chrono::DateTime::parse_from_rfc3339("2025-06-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let slot_window = Selection {
        site_id: Some(uuid::Uuid::nil()),
        parameter_id: Some(uuid::Uuid::nil()),
        from: Some(at),
        to: Some(at),
        ..Default::default()
    };
    let window = sql(&slot_window);
    assert!(window.contains(r#""r"."site_id" ="#), "{window}");
    assert!(window.contains(r#""r"."parameter_id" ="#), "{window}");
    assert!(window.contains(r#""r"."time" >="#), "{window}");
    assert!(window.contains(r#""r"."time" <="#), "{window}");

    // A key names a replicate or it names the whole group; the group form leaves the index out
    // rather than matching every index.
    let keys = Selection {
        keys: vec![
            SelectionKey {
                stream_id: uuid::Uuid::nil(),
                time: at,
                replicate_index: Some(1),
                value: None,
            },
            SelectionKey {
                stream_id: uuid::Uuid::nil(),
                time: at,
                replicate_index: None,
                value: None,
            },
        ],
        ..Default::default()
    };
    let keyed = sql(&keys);
    assert_eq!(keyed.matches(r#""r"."stream_id" ="#).count(), 2, "{keyed}");
    assert_eq!(
        keyed.matches(r#""r"."replicate_index" = 1"#).count(),
        1,
        "{keyed}"
    );
    assert!(keyed.contains(" OR "), "{keyed}");
}

#[test]
fn a_correction_naming_values_per_key_groups_them_by_stream() {
    let at = chrono::Utc::now();
    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    let selection = Selection {
        keys: vec![
            SelectionKey {
                stream_id: a,
                time: at,
                replicate_index: Some(0),
                value: Some(1.5),
            },
            SelectionKey {
                stream_id: a,
                time: at,
                replicate_index: Some(1),
                value: Some(2.5),
            },
            SelectionKey {
                stream_id: b,
                time: at,
                replicate_index: Some(0),
                value: Some(3.5),
            },
        ],
        ..Default::default()
    };
    let by_stream = keyed_corrections(&selection).unwrap().unwrap();
    assert_eq!(by_stream.len(), 2, "one entry per stream");
    assert_eq!(by_stream[&a].len(), 2);
    assert_eq!(by_stream[&a][0].2, serde_json::json!({ "raw_value": 1.5 }));
    assert_eq!(by_stream[&b].len(), 1);
}

#[test]
fn a_selection_with_no_values_is_one_assertion_over_a_predicate() {
    let selection = Selection {
        stream_id: Some(uuid::Uuid::new_v4()),
        ..Default::default()
    };
    assert!(keyed_corrections(&selection).unwrap().is_none());
}

#[test]
fn mixing_keys_with_and_without_a_value_is_refused() {
    let at = chrono::Utc::now();
    let stream_id = uuid::Uuid::new_v4();
    let selection = Selection {
        keys: vec![
            SelectionKey {
                stream_id,
                time: at,
                replicate_index: Some(0),
                value: Some(1.0),
            },
            SelectionKey {
                stream_id,
                time: at,
                replicate_index: Some(1),
                value: None,
            },
        ],
        ..Default::default()
    };
    assert!(keyed_corrections(&selection).is_err());
}

#[test]
fn a_per_key_correction_names_the_replicate_each_value_belongs_to() {
    let selection = Selection {
        keys: vec![SelectionKey {
            stream_id: uuid::Uuid::new_v4(),
            time: chrono::Utc::now(),
            replicate_index: None,
            value: Some(1.0),
        }],
        ..Default::default()
    };
    assert!(keyed_corrections(&selection).is_err());
}

#[test]
fn a_selection_can_name_one_visit() {
    use super::Selection;
    let sql = |selection: &Selection| {
        sea_orm::sea_query::Query::select()
            .expr(sea_orm::sea_query::Expr::val(1))
            .cond_where(selection.condition().unwrap())
            .to_string(sea_orm::sea_query::PostgresQueryBuilder)
    };
    let event = uuid::Uuid::nil();
    let by_event = Selection {
        collection_event_id: Some(event),
        ..Default::default()
    };
    assert!(
        sql(&by_event).ends_with(
            r#"WHERE "r"."collection_event_id" = '00000000-0000-0000-0000-000000000000'"#
        )
    );

    // A visit narrowed to one parameter is still a selection, and the slot rule still holds.
    let one_parameter = Selection {
        collection_event_id: Some(event),
        site_id: Some(uuid::Uuid::nil()),
        parameter_id: Some(uuid::Uuid::nil()),
        ..Default::default()
    };
    let narrowed = sql(&one_parameter);
    assert!(
        narrowed.contains(r#""r"."collection_event_id" ="#),
        "{narrowed}"
    );
    assert!(narrowed.contains(r#""r"."site_id" ="#), "{narrowed}");
    assert!(narrowed.contains(r#""r"."parameter_id" ="#), "{narrowed}");
}

#[test]
fn a_detach_makes_the_slot_manual_until_an_input_moves_or_it_is_returned() {
    use super::{Owner, slot_owner};
    let t = |s: &str| {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .with_timezone(&chrono::Utc)
    };
    assert_eq!(slot_owner(&[], None), Owner::Tool);
    assert_eq!(
        slot_owner(&[(Kind::Chain, t("2025-06-01T00:00:00Z"))], None),
        Owner::Tool
    );
    let detached = [(Kind::Detach, t("2025-06-02T00:00:00Z"))];
    assert_eq!(slot_owner(&detached, None), Owner::Manual);
    assert_eq!(
        slot_owner(&detached, Some(t("2025-06-01T12:00:00Z"))),
        Owner::Manual,
        "an input edit before the detach does not re-engage"
    );
    assert_eq!(
        slot_owner(&detached, Some(t("2025-06-03T00:00:00Z"))),
        Owner::Tool,
        "an input edit after the detach re-engages the tool"
    );
    let returned = [
        (Kind::Return, t("2025-06-04T00:00:00Z")),
        (Kind::Detach, t("2025-06-02T00:00:00Z")),
    ];
    assert_eq!(slot_owner(&returned, None), Owner::Tool);
}

#[test]
fn the_fold_takes_the_newest_live_decision_per_column_and_the_born_state_otherwise() {
    use super::{FoldEntry, ProjectedColumns, projected_state};
    let t = |s: &str| {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .with_timezone(&chrono::Utc)
    };
    let entry = |kind, at, new| FoldEntry {
        kind,
        at: t(at),
        new,
    };
    assert_eq!(projected_state(&[]), ProjectedColumns::default());
    assert_eq!(
        projected_state(&[entry(
            Kind::Flag,
            "2025-06-02T00:00:00Z",
            serde_json::json!({ "reason": "spike" })
        )]),
        ProjectedColumns {
            is_flagged: true,
            flag_reason: Some("spike".to_string()),
            ..Default::default()
        }
    );
    // Newest first: the unflag stands and the flag under it is not consulted.
    assert_eq!(
        projected_state(&[
            entry(Kind::Unflag, "2025-06-03T00:00:00Z", serde_json::json!({})),
            entry(
                Kind::Flag,
                "2025-06-02T00:00:00Z",
                serde_json::json!({ "reason": "spike" })
            ),
        ]),
        ProjectedColumns::default()
    );
    // A withdraw with no explicit instant is stamped at the decision.
    assert_eq!(
        projected_state(&[entry(
            Kind::Withdraw,
            "2025-06-04T00:00:00Z",
            serde_json::json!({ "reason": "absent from source window" })
        )]),
        ProjectedColumns {
            withdrawn_at: Some(t("2025-06-04T00:00:00Z")),
            withdrawn_reason: Some("absent from source window".to_string()),
            ..Default::default()
        }
    );
    assert_eq!(
        projected_state(&[
            entry(
                Kind::Reassert,
                "2025-06-05T00:00:00Z",
                serde_json::json!({})
            ),
            entry(
                Kind::Withdraw,
                "2025-06-04T00:00:00Z",
                serde_json::json!({ "reason": "gone" })
            ),
        ]),
        ProjectedColumns::default()
    );
    // A reject withdraws and clears the unverified stamp in one decision.
    assert_eq!(
        projected_state(&[entry(
            Kind::Reject,
            "2025-06-06T00:00:00Z",
            serde_json::json!({ "reason": "rejected" })
        )]),
        ProjectedColumns {
            withdrawn_at: Some(t("2025-06-06T00:00:00Z")),
            withdrawn_reason: Some("rejected".to_string()),
            unverified: false,
            ..Default::default()
        }
    );
    assert_eq!(
        projected_state(&[entry(
            Kind::UnverifiedEntry,
            "2025-06-07T00:00:00Z",
            serde_json::json!({})
        )]),
        ProjectedColumns {
            unverified: true,
            ..Default::default()
        }
    );
    // Kinds that project nothing folded leave every column at the born state.
    for k in [
        Kind::Curve,
        Kind::InstrumentPin,
        Kind::CalibrationPin,
        Kind::ValueCorrection,
        Kind::SlotMove,
        Kind::Chain,
        Kind::Detach,
        Kind::Return,
    ] {
        assert_eq!(
            projected_state(&[entry(k, "2025-06-08T00:00:00Z", serde_json::json!({}))]),
            ProjectedColumns::default(),
            "{k:?}"
        );
    }
}

#[test]
fn a_rollback_asserts_the_columns_it_restores_and_the_decision_it_undid_is_not_folded() {
    use super::{FoldEntry, ProjectedColumns, projected_state};
    let t = |s: &str| {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .with_timezone(&chrono::Utc)
    };
    // The rolled-back decision is not in the list: `rolled_back_by` takes it out of the fold.
    let rollback = FoldEntry {
        kind: Kind::Rollback,
        at: t("2025-06-09T00:00:00Z"),
        new: serde_json::json!({
            "columns": { "is_flagged": false, "flag_reason": null },
            "of": "00000000-0000-0000-0000-000000000000"
        }),
    };
    assert_eq!(
        projected_state(std::slice::from_ref(&rollback)),
        ProjectedColumns::default()
    );
    // It asserts only the columns it names, so an unrelated live decision still stands.
    let withdrawn = FoldEntry {
        kind: Kind::Withdraw,
        at: t("2025-06-08T00:00:00Z"),
        new: serde_json::json!({ "reason": "gone" }),
    };
    assert_eq!(
        projected_state(&[rollback, withdrawn]),
        ProjectedColumns {
            withdrawn_at: Some(t("2025-06-08T00:00:00Z")),
            withdrawn_reason: Some("gone".to_string()),
            ..Default::default()
        }
    );
}

#[test]
fn the_drift_statement_folds_every_owned_column_and_repairs_none() {
    let sql = super::inconsistent_rows().to_string(sea_orm::sea_query::PostgresQueryBuilder);
    for col in super::FOLDED_COLUMNS {
        assert!(sql.contains(&format!("'{col}'")), "{col} is not folded");
    }
    assert!(sql.contains(r#""d"."rolled_back_by" IS NULL"#));
    assert!(sql.contains(
        r#""d"."replicate_index" IS NULL OR "d"."replicate_index" = "c"."replicate_index""#
    ));
    // A row with no decision at all is still a candidate when a column left the born state.
    assert!(sql.contains(r#""r"."is_flagged" IS TRUE"#));
    assert!(sql.contains(r#""r"."flag_reason" IS NOT NULL"#));
    for write in ["UPDATE ", "DELETE ", "INSERT "] {
        assert!(
            !sql.contains(write),
            "the sweep reports, it does not repair"
        );
    }
}

/// Expected behaviour: `supersedes` names exactly the family it supersedes, and a kind that has no
/// family supersedes nothing rather than every decision on the key. The text it replaced bound an
/// array and matched with `= ANY($1)`, which on an empty array is false; the built form renders
/// sea-query's own empty-`IN` falsehood.
#[test]
fn a_kind_supersedes_its_own_family_and_a_family_less_kind_supersedes_nothing() {
    use super::{Kind, family_kinds, supersedes};
    use sea_orm::sea_query::{Alias, PostgresQueryBuilder, Query};

    assert_eq!(family_kinds(Kind::Flag), ["flag", "unflag"]);
    assert_eq!(family_kinds(Kind::Unflag), family_kinds(Kind::Flag));
    assert_eq!(
        family_kinds(Kind::Withdraw),
        ["withdraw", "reassert", "reject"]
    );
    assert_eq!(Kind::Rollback.family(), None);
    // A kind that stands alone supersedes nothing, which is what its own comment in `family` says
    // it must not do. Each carries a family name of its own and none is among the candidates, so
    // the list comes back empty: a second reprocess does not hide the reprocess before it.
    for kind in [
        Kind::Rollback,
        Kind::CurveRetire,
        Kind::FormulaTransition,
        Kind::CurveRecompose,
        Kind::DerivedComputed,
        Kind::Reprocess,
        Kind::Retag,
        Kind::Attribution,
    ] {
        assert!(family_kinds(kind).is_empty(), "{kind:?}");
    }

    let render = |kind: Kind| {
        let c = Alias::new("c");
        Query::select()
            .expr(supersedes(kind, &c))
            .from_as(crate::routes::private::readings::models::Entity, c.clone())
            .to_owned()
            .to_string(PostgresQueryBuilder)
    };
    let flag = render(Kind::Flag);
    assert!(
        flag.contains(r#""d"."kind" IN ('flag', 'unflag')"#),
        "{flag}"
    );
    assert!(
        flag.contains(r#""replicate_index" IS NOT DISTINCT FROM "c"."replicate_index""#),
        "the composite key matches a NULL index on both sides: {flag}"
    );
    let alone = render(Kind::Reprocess);
    assert!(
        alone.contains("1 = 2"),
        "an empty family matches no decision, the way the array it replaced did: {alone}"
    );
}

/// Expected behaviour: the three boolean predicates that are not pinned elsewhere render as
/// `IS TRUE` / `IS NOT TRUE`, never `= TRUE`. A nullable boolean compared with `=` is NULL rather
/// than false, which would project `flagged` as NULL on a never-flagged row and drop the CASE arm.
#[test]
fn the_boolean_predicates_read_a_null_flag_as_false() {
    use sea_orm::QueryTrait;
    use sea_orm::sea_query::PostgresQueryBuilder;

    let preview = super::preview_rows()
        .build(sea_orm::DatabaseBackend::Postgres)
        .to_string();
    assert!(
        preview.contains(r#""is_flagged" IS TRUE"#),
        "the projected flag reads a NULL as false: {preview}"
    );

    let kept = sea_orm::sea_query::Query::select()
        .expr(super::kept_reason())
        .from(crate::routes::private::readings::models::Entity)
        .to_owned()
        .to_string(PostgresQueryBuilder);
    assert!(
        kept.contains(r#""is_flagged" IS TRUE"#),
        "the CASE arm reads a NULL as false: {kept}"
    );

    let curved = sea_orm::sea_query::Query::select()
        .expr(sea_orm::sea_query::Expr::cust("1"))
        .from(crate::routes::private::readings::models::Entity)
        .cond_where(super::curated_or_curved(false))
        .to_owned()
        .to_string(PostgresQueryBuilder);
    assert!(
        curved.contains(r#""is_flagged" IS TRUE"#),
        "a replace keeps a flagged replicate whose flag is stored NULL nowhere: {curved}"
    );

    for sql in [&preview, &kept, &curved] {
        assert!(!sql.contains("= TRUE"), "{sql}");
        assert!(!sql.contains("IS $"), "a bound keyword is not SQL: {sql}");
    }
}

#[test]
fn only_an_intern_enters_a_pending_measurement() {
    use super::entry_state;
    use crate::common::authz::Role;
    assert_eq!(
        entry_state(Some(&Role::Intern)),
        Some(Kind::UnverifiedEntry)
    );
    for role in [
        Role::River,
        Role::Manager,
        Role::Administrator,
        Role::Unknown("offline_access".to_string()),
    ] {
        assert_eq!(entry_state(Some(&role)), None, "{role:?}");
    }
    // An API token has bits, not a level: it enters verified, as it always did.
    assert_eq!(entry_state(None), None);
}

#[test]
fn a_value_computed_from_a_pending_measurement_is_pending_itself() {
    use super::entry_kind;
    use crate::common::authz::Role;
    // The chain runs as whoever triggered it, and the inputs decide, not the level.
    for role in [
        None,
        Some(Role::River),
        Some(Role::Manager),
        Some(Role::Administrator),
    ] {
        assert_eq!(
            entry_kind(true, role.as_ref()),
            Some(Kind::UnverifiedEntry),
            "{role:?}"
        );
    }
    assert_eq!(entry_kind(false, Some(&Role::Manager)), None);
    assert_eq!(
        entry_kind(false, Some(&Role::Intern)),
        Some(Kind::UnverifiedEntry),
        "verified inputs leave the writer's level deciding"
    );
}

#[test]
fn sync_owns_the_measurement_and_never_a_judgement() {
    use super::{is_judgement, judgement_kinds};
    for k in [
        Kind::Flag,
        Kind::Curve,
        Kind::CalibrationPin,
        Kind::InstrumentPin,
        Kind::UnverifiedEntry,
        Kind::Verify,
        Kind::Reject,
    ] {
        assert!(is_judgement(k), "{k:?} is a person's ruling");
    }
    // The measurement, and the record's own bookkeeping, are not judgements: a re-send
    // corrects a value and retracts a row it no longer asserts without anyone ruling again.
    for k in [
        Kind::Withdraw,
        Kind::Reassert,
        Kind::ValueCorrection,
        Kind::Unflag,
        Kind::SlotMove,
        Kind::Chain,
        Kind::Detach,
        Kind::Return,
        Kind::Rollback,
    ] {
        assert!(!is_judgement(k), "{k:?} is not a ruling sync must respect");
    }
    let mut judged = judgement_kinds();
    judged.sort_unstable();
    let mut expected = [
        Kind::Flag,
        Kind::Curve,
        Kind::CalibrationPin,
        Kind::InstrumentPin,
        Kind::UnverifiedEntry,
        Kind::Verify,
        Kind::Reject,
    ]
    .map(Kind::as_str)
    .to_vec();
    expected.sort_unstable();
    assert_eq!(
        judged, expected,
        "the statement's kinds are the rulings and no other"
    );
}

#[test]
fn the_judgement_statement_names_every_judged_kind_and_no_other() {
    let sql = sea_orm::sea_query::Query::select()
        .expr(super::live_judgements("r"))
        .to_string(sea_orm::sea_query::PostgresQueryBuilder);
    for k in super::ALL_KINDS {
        let named = sql.contains(&format!("'{}'", k.as_str()));
        assert_eq!(named, super::is_judgement(k), "{k:?}");
    }
    assert!(sql.contains(r#""d"."rolled_back_by" IS NULL"#), "{sql}");
    assert!(
        sql.contains(
            r#""d"."replicate_index" IS NULL OR "d"."replicate_index" = "r"."replicate_index""#
        ),
        "{sql}"
    );
}

#[test]
fn a_value_correction_is_per_row_only() {
    assert!(Kind::ValueCorrection.per_row_only());
    assert!(!Kind::Flag.per_row_only());
    assert!(!Kind::Withdraw.per_row_only());
}

/// The keyed recorder reads `k.t`, `k.ri` and `k.n`, so the key set has to name them. An
/// `unnest(...) AS k` with no column list names all three `unnest` instead, and every reference
/// to them fails.
#[test]
fn the_key_set_names_the_column_each_key_is_read_by() {
    let sql = super::key_set(
        vec![chrono::DateTime::UNIX_EPOCH],
        vec![0],
        vec!["{}".to_string()],
    )
    .to_string(sea_orm::sea_query::PostgresQueryBuilder);
    for column in [r#"AS "t""#, r#"AS "ri""#, r#"AS "n""#] {
        assert!(sql.contains(column), "{column} missing from {sql}");
    }
    assert_eq!(sql.matches("unnest").count(), 3, "{sql}");
    assert!(
        !sql.contains("text[]"),
        "the times bind as timestamps, not text: {sql}"
    );
}

#[test]
fn the_supersedes_lookup_names_its_columns_and_reads_the_live_row_of_the_family() {
    let sql = sea_orm::sea_query::Query::select()
        .expr(super::supersedes(
            Kind::Flag,
            &sea_orm::sea_query::Alias::new("t"),
        ))
        .to_owned()
        .to_string(sea_orm::sea_query::PostgresQueryBuilder);

    assert!(
        sql.contains(r#""d"."stream_id" = "t"."stream_id""#),
        "{sql}"
    );
    assert!(sql.contains(r#""d"."time" = "t"."time""#), "{sql}");
    assert!(
        sql.contains(r#""d"."replicate_index" IS NOT DISTINCT FROM "t"."replicate_index""#),
        "{sql}"
    );
    assert!(sql.contains(r#""d"."rolled_back_by" IS NULL"#), "{sql}");
    // The whole family, so an unflag supersedes the flag it answers.
    for kind in super::family_kinds(Kind::Flag) {
        assert!(sql.contains(&format!("'{kind}'")), "{kind} missing: {sql}");
    }
    assert!(
        sql.contains(r#"ORDER BY "d"."at" DESC, "d"."id" DESC LIMIT 1"#),
        "{sql}"
    );
}

/// Scenario: a write setting one reading column records each row it is about to move.
/// Expected behaviour: the ledger selects the rows the condition names, records the column's
/// stored value and the new one, the reason, and no job when none ran it.
#[test]
fn test_column_ledger_records_the_column_on_both_sides() {
    use crate::routes::private::readings::models::Column;
    use sea_orm::ColumnTrait;

    let sql = super::column_ledger(
        Kind::Attribution,
        sea_orm::Condition::all().add(Column::DeploymentId.eq(uuid::Uuid::nil())),
        Column::DeploymentId,
        sea_orm::sea_query::Expr::val(Option::<uuid::Uuid>::None),
        "deployment rolled back",
    )
    .to_string(sea_orm::sea_query::PostgresQueryBuilder);
    assert!(
        sql.starts_with(r#"INSERT INTO "reading_decisions""#),
        "{sql}"
    );
    assert!(sql.contains("'attribution'"), "{sql}");
    assert!(sql.contains("'deployment rolled back'"), "{sql}");
    assert!(
        sql.contains(r#"jsonb_build_object('deployment_id', "readings"."deployment_id")"#),
        "{sql}"
    );
    assert!(
        sql.contains("jsonb_build_object('deployment_id', NULL)"),
        "{sql}"
    );
    assert!(
        sql.contains(
            r#"WHERE "readings"."deployment_id" = '00000000-0000-0000-0000-000000000000'"#
        ),
        "{sql}"
    );
}
