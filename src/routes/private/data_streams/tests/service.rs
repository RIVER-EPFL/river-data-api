use super::*;
use crate::routes::private::data_streams::models::ColumnAssignment;
use sea_orm::QueryTrait;

#[test]
fn nomis_is_refused_and_says_why() {
    let reason = pairing_refusal("nomis").expect("NOMIS is refused");
    assert!(reason.contains("no zone"), "{reason}");
    assert!(reason.contains("ADR 0004"), "{reason}");
    assert!(
        pairing_refusal("NOMIS").is_some(),
        "the source system is compared without regard to case"
    );
}

#[test]
fn every_other_source_pairs() {
    for source in ["cnet", "metalp", "vaisala", "api", "grab_sample"] {
        assert!(pairing_refusal(source).is_none(), "{source} pairs");
    }
}

fn declaration(columns: &[&str]) -> river_data_core::models::ReplicateSpec {
    river_data_core::models::ReplicateSpec {
        source_columns: columns.iter().map(ToString::to_string).collect(),
        portal_mean_column: Some("DOC_avg_ppb".to_string()),
        portal_sd_column: Some("DOC_sd_ppb".to_string()),
        curve_ref_column: Some("doc_std_curve_id".to_string()),
        calc: Some("calcDOCavg".to_string()),
    }
}

fn spec(columns: &[&str]) -> ReplicateSpec {
    ReplicateSpec {
        declared: declaration(columns),
        assignments: Vec::new(),
    }
}

#[test]
fn a_valid_spec_roundtrips_through_metadata() {
    let s = spec(&["DOC_rep_1", "DOC_rep_2", "DOC_rep_3"]);
    validate_declaration(&s.declared, Some("spot")).unwrap();
    let mut metadata = serde_json::json!({"hierarchy": {"site": "DGT"}});
    s.embed(&mut metadata).unwrap();
    let parsed = ReplicateSpec::from_metadata(&metadata).unwrap();
    assert_eq!(parsed.declared.source_columns, s.declared.source_columns);
    assert_eq!(
        parsed.declared.portal_mean_column.as_deref(),
        Some("DOC_avg_ppb")
    );
    assert_eq!(metadata["hierarchy"]["site"], "DGT");
}

#[test]
fn a_single_member_is_refused() {
    assert!(validate_declaration(&declaration(&["DOC_rep_1"]), Some("spot")).is_err());
}

#[test]
fn duplicate_members_are_refused() {
    assert!(validate_declaration(&declaration(&["DOC_rep_1", "DOC_rep_1"]), Some("spot")).is_err());
}

/// A spec stored before pinning resolves to each declared column at its
/// position, which is the index its readings were stored under. The sync
/// client reads the same metadata through `ColumnAssignment::from_metadata`
/// in `river-data-core` and must land on this same mapping, or a legacy
/// stream's replicates are indexed one way on write and another on read.
#[test]
fn an_unpinned_spec_resolves_to_column_positions() {
    let resolved = spec(&["DOC_rep_1", "DOC_rep_2", "DOC_rep_3"]).column_assignments();
    assert_eq!(
        resolved,
        vec![
            ColumnAssignment {
                column: "DOC_rep_1".to_string(),
                index: 0,
                retired: false,
            },
            ColumnAssignment {
                column: "DOC_rep_2".to_string(),
                index: 1,
                retired: false,
            },
            ColumnAssignment {
                column: "DOC_rep_3".to_string(),
                index: 2,
                retired: false,
            },
        ]
    );
}

#[test]
fn a_non_spot_stream_cannot_declare_replicates() {
    assert!(
        validate_declaration(
            &declaration(&["DOC_rep_1", "DOC_rep_2"]),
            Some("continuous")
        )
        .is_err()
    );
    assert!(validate_declaration(&declaration(&["DOC_rep_1", "DOC_rep_2"]), None).is_err());
}

/// Scenario: the three reads behind `/streams/{id}/stats` and `/streams/{id}/preview`.
/// Expected behaviour: they are built from the readings entity, and the preview counts instants
/// rather than replicates, so a limit of one still returns a whole replicate group.
mod stream_reads {
    use super::super::{latest_raw_value_query, preview_query, stream_stats_query};
    use sea_orm::QueryTrait;
    use sea_orm::sea_query::PostgresQueryBuilder;
    use uuid::Uuid;

    #[test]
    fn test_stream_stats_counts_the_withdrawn_rows_apart_from_the_rest() {
        let sql = stream_stats_query(Uuid::nil())
            .into_query()
            .to_string(PostgresQueryBuilder);
        assert!(
            str::contains(
                &sql,
                r#"COUNT(*) FILTER (WHERE withdrawn_at IS NOT NULL) AS "withdrawn""#
            ),
            "{sql}"
        );
        for expected in [
            r#"COUNT("readings"."time") AS "count""#,
            r#"MIN("readings"."time") AS "min_time""#,
            r#"MAX("readings"."time") AS "max_time""#,
            r#"FROM "readings" WHERE "readings"."stream_id" ="#,
        ] {
            assert!(str::contains(&sql, expected), "{expected} missing: {sql}");
        }
    }

    #[test]
    fn test_the_latest_raw_value_is_the_newest_row_of_the_stream() {
        let sql = latest_raw_value_query(Uuid::nil())
            .into_query()
            .to_string(PostgresQueryBuilder);
        assert!(
            str::starts_with(&sql, r#"SELECT "readings"."raw_value" FROM "readings""#),
            "{sql}"
        );
        assert!(
            str::ends_with(&sql, r#"ORDER BY "readings"."time" DESC LIMIT 1"#),
            "{sql}"
        );
    }

    #[test]
    fn test_the_preview_limit_counts_instants_not_replicates() {
        let sql = preview_query(Uuid::nil(), 3).to_string(PostgresQueryBuilder);
        // The limit sits on the DISTINCT-time subquery, so three instants come back whole.
        assert!(
            str::contains(
                &sql,
                r#"SELECT DISTINCT "time" FROM "readings" WHERE "readings"."stream_id" ="#
            ),
            "{sql}"
        );
        assert!(
            str::contains(&sql, r#"ORDER BY "time" DESC LIMIT 3) AS "t""#),
            "{sql}"
        );
        assert!(
            str::ends_with(
                &sql,
                r#"ORDER BY "readings"."time" DESC, "readings"."replicate_index" ASC"#
            ),
            "{sql}"
        );
    }
}

/// Scenario: two requests pair the same stream.
/// Expected behaviour: the write carries `site_parameter_id IS NULL`, so the second affects no row
/// and is refused instead of repointing a stream whose readings are already attributed.
#[test]
fn the_pairing_claim_only_applies_while_the_stream_is_unpaired() {
    let sql = claim_stream(Uuid::nil(), Uuid::nil(), chrono::Utc::now().into())
        .build(sea_orm::DatabaseBackend::Postgres)
        .to_string();

    assert!(sql.contains(r#"UPDATE "data_streams""#), "{sql}");
    assert!(sql.contains(r#""site_parameter_id" IS NULL"#), "{sql}");
    assert!(sql.contains(r#""id" ="#), "{sql}");
    assert!(sql.contains(r#""paired_at" ="#), "{sql}");
    assert!(sql.contains(r#""updated_at" ="#), "{sql}");
}

/// A source that re-publishes an older instant must not drag the cursor backwards.
#[test]
fn the_cursor_advance_never_moves_the_cursor_back() {
    let sql = advance_cursor(Uuid::nil(), chrono::Utc::now().into())
        .build(sea_orm::DatabaseBackend::Postgres)
        .to_string();

    assert!(sql.contains("GREATEST(COALESCE("), "{sql}");
    assert!(sql.contains(r#""last_data_time""#), "{sql}");
    assert!(sql.contains(r#""id" ="#), "{sql}");
}

#[test]
fn test_declared_instrument_granularity_reads_both_shapes() {
    use river_data_core::models::InstrumentGranularity;
    assert_eq!(
        declared_instrument_granularity(&serde_json::json!({
            INSTRUMENT_GRANULARITY_KEY: "per_parameter"
        })),
        Some(InstrumentGranularity::PerParameter)
    );
    assert_eq!(
        declared_instrument_granularity(&serde_json::json!({
            INSTRUMENT_GRANULARITY_KEY: "per_site_parameter"
        })),
        Some(InstrumentGranularity::PerSiteParameter)
    );
    assert_eq!(
        declared_instrument_granularity(&serde_json::json!({})),
        None
    );
    assert_eq!(
        declared_instrument_granularity(
            &serde_json::json!({ INSTRUMENT_GRANULARITY_KEY: "per_station" })
        ),
        None
    );
}

mod pinning {
    use super::super::pin_assignments;
    use super::spec;
    use crate::routes::private::data_streams::models::{ColumnAssignment, ReplicateSpec};

    fn columns(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    fn pinned(names: &[&str]) -> ReplicateSpec {
        let mut prior = spec(names);
        prior.assignments = pin_assignments(None, &columns(names)).expect("first registration");
        prior
    }

    fn index_of(assignments: &[ColumnAssignment], column: &str) -> (i16, bool) {
        let a = assignments
            .iter()
            .find(|a| a.column == column)
            .unwrap_or_else(|| panic!("{column} is assigned: {assignments:?}"));
        (a.index, a.retired)
    }

    #[test]
    fn test_a_first_registration_numbers_the_columns_in_order() {
        let assigned =
            pin_assignments(None, &columns(&["DOC_1", "DOC_2", "DOC_3"])).expect("pinned");
        assert_eq!(index_of(&assigned, "DOC_1"), (0, false));
        assert_eq!(index_of(&assigned, "DOC_3"), (2, false));
    }

    #[test]
    fn test_a_reordered_family_keeps_each_column_on_its_index() {
        let prior = pinned(&["DOC_1", "DOC_2", "DOC_3"]);
        let assigned =
            pin_assignments(Some(&prior), &columns(&["DOC_3", "DOC_1", "DOC_2"])).expect("pinned");
        assert_eq!(index_of(&assigned, "DOC_1"), (0, false));
        assert_eq!(index_of(&assigned, "DOC_2"), (1, false));
        assert_eq!(index_of(&assigned, "DOC_3"), (2, false));
    }

    #[test]
    fn test_an_added_column_takes_the_next_index() {
        let prior = pinned(&["DOC_1", "DOC_2"]);
        let assigned =
            pin_assignments(Some(&prior), &columns(&["DOC_4", "DOC_1", "DOC_2"])).expect("pinned");
        assert_eq!(index_of(&assigned, "DOC_4"), (2, false));
        assert_eq!(assigned.len(), 3);
    }

    #[test]
    fn test_a_dropped_column_is_retired_on_its_index_and_returns_to_it() {
        let prior = pinned(&["DOC_1", "DOC_2", "DOC_3"]);
        let dropped = pin_assignments(Some(&prior), &columns(&["DOC_1", "DOC_3"])).expect("pinned");
        assert_eq!(index_of(&dropped, "DOC_2"), (1, true));

        let mut retired = spec(&["DOC_1", "DOC_3"]);
        retired.assignments = dropped;
        let back = pin_assignments(Some(&retired), &columns(&["DOC_1", "DOC_2", "DOC_3"]))
            .expect("pinned");
        assert_eq!(index_of(&back, "DOC_2"), (1, false));
        assert_eq!(back.len(), 3, "a returning column is not added twice");
    }

    #[test]
    fn test_a_retired_index_is_never_reused() {
        let prior = pinned(&["DOC_1", "DOC_2", "DOC_3"]);
        let mut retired = spec(&["DOC_1", "DOC_2"]);
        retired.assignments =
            pin_assignments(Some(&prior), &columns(&["DOC_1", "DOC_2"])).expect("pinned");
        let assigned = pin_assignments(Some(&retired), &columns(&["DOC_1", "DOC_2", "DOC_9"]))
            .expect("pinned");
        // DOC_3 keeps 2 while retired, so the new column takes 3
        assert_eq!(index_of(&assigned, "DOC_3"), (2, true));
        assert_eq!(index_of(&assigned, "DOC_9"), (3, false));
    }

    #[test]
    fn test_a_removal_and_an_addition_in_one_step_is_refused_as_an_ambiguous_rename() {
        let prior = pinned(&["DOC_1", "DOC_2"]);
        let err =
            pin_assignments(Some(&prior), &columns(&["DOC_1", "DOC_X"])).expect_err("refused");
        assert!(err.to_string().contains("ambiguous"), "{err}");
    }

    #[test]
    fn test_a_column_named_twice_is_assigned_once_when_already_known() {
        let prior = pinned(&["DOC_1", "DOC_2"]);
        let assigned =
            pin_assignments(Some(&prior), &columns(&["DOC_1", "DOC_1", "DOC_2"])).expect("pinned");
        assert_eq!(assigned.len(), 2);
    }
}

mod family_retag {
    use super::super::refuse_family_retag;

    #[test]
    fn test_a_retag_touching_no_family_is_allowed() {
        assert!(refuse_family_retag(&[], "continuous").is_ok());
    }

    #[test]
    fn test_a_retag_touching_a_family_is_refused_naming_it() {
        let err =
            refuse_family_retag(&["S01:DOC:reps".to_string()], "continuous").expect_err("refused");
        assert!(err.to_string().contains("S01:DOC:reps"), "{err}");
        assert!(err.to_string().contains("'continuous'"), "{err}");
    }
}

mod decimal_places {
    use super::super::declared_decimal_places;
    use serde_json::json;

    #[test]
    fn test_the_declared_places_are_read_from_metadata() {
        assert_eq!(
            declared_decimal_places(&json!({ "decimal_places": 2 })),
            Some(2)
        );
    }

    #[test]
    fn test_no_declaration_or_an_unreadable_one_is_none() {
        assert_eq!(declared_decimal_places(&json!({})), None);
        assert_eq!(declared_decimal_places(&json!(null)), None);
        assert_eq!(
            declared_decimal_places(&json!({ "decimal_places": "2" })),
            None
        );
        assert_eq!(
            declared_decimal_places(&json!({ "decimal_places": 70000 })),
            None
        );
    }
}

/// Expected behaviour: a merge announces every site it moved readings at under both parameters, the
/// absorbed one whose series empties and the surviving one whose series grows.
#[test]
fn test_slot_move_tally_announces_the_source_and_the_target_slot() {
    let (site_a, site_b) = (Uuid::from_u128(1), Uuid::from_u128(2));
    let (source, target) = (Uuid::from_u128(10), Uuid::from_u128(20));
    let announced: Vec<(Option<Uuid>, Option<Uuid>, usize)> =
        slot_move_tally(&[(site_a, 3), (site_b, 0)], source, target)
            .events()
            .into_iter()
            .filter_map(|event| match event {
                crate::common::AppEvent::DataIngested {
                    site_id,
                    parameter_id,
                    count,
                    ..
                } => Some((site_id, parameter_id, count)),
                _ => None,
            })
            .collect();
    assert_eq!(
        announced,
        vec![
            (Some(site_a), Some(source), 3),
            (Some(site_a), Some(target), 3),
        ],
        "a site with nothing to move announces nothing"
    );
}
