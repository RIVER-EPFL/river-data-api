use super::*;

#[test]
fn stats_of_a_triplicate() {
    let s = group_stats(&[1.0, 2.0, 3.0]);
    assert_eq!(s.n, 3);
    assert!((s.mean.unwrap() - 2.0).abs() < 1e-12);
    assert!((s.sd.unwrap() - 1.0).abs() < 1e-12);
}

#[test]
fn a_singleton_has_no_sd() {
    let s = group_stats(&[5.0]);
    assert_eq!(s.n, 1);
    assert_eq!(s.mean, Some(5.0));
    assert_eq!(s.sd, None);
}

#[test]
fn agreement_is_relative_with_an_absolute_floor() {
    assert!(stats_agree(Some(1000.0), Some(1000.005), DEFAULT_REL_TOL));
    assert!(!stats_agree(Some(1000.0), Some(1001.0), DEFAULT_REL_TOL));
    assert!(stats_agree(Some(0.0), Some(5e-5), DEFAULT_REL_TOL));
    assert!(!stats_agree(Some(0.0), Some(0.5), DEFAULT_REL_TOL));
}

#[test]
fn sd_tolerates_portal_float_noise_but_not_percent_level_drift() {
    // Real cnet example: stored sd 12.1798 vs recomputed 12.17994 (FLOAT noise, passes).
    assert!(stats_agree_with(
        Some(12.179800033569336),
        Some(12.179940200962006),
        SD_REL_TOL,
        SD_ABS_TOL
    ));
    // An n-divisor sd against the sample sd (sqrt(2/3) off) is a real finding and still holds.
    assert!(!stats_agree_with(
        Some(68.22),
        Some(83.55),
        SD_REL_TOL,
        SD_ABS_TOL
    ));
}

#[test]
fn quantization_of_the_portal_cell_is_not_a_mismatch() {
    // A 2dp-stored portal mean differs from the true mean by up to half the quantum.
    assert!(stats_agree(
        Some(147.33),
        Some(147.333_333_333_333_3),
        DEFAULT_REL_TOL
    ));
    // A real disagreement sits far above the quantum and still holds.
    assert!(!stats_agree_with(
        Some(11.09),
        Some(13.5769),
        SD_REL_TOL,
        SD_ABS_TOL
    ));
}

#[test]
fn a_missing_side_is_not_a_mismatch() {
    assert!(stats_agree(None, Some(1.0), DEFAULT_REL_TOL));
    assert!(stats_agree(Some(1.0), None, DEFAULT_REL_TOL));
    assert!(stats_agree(None, None, DEFAULT_REL_TOL));
}

fn classify_case(expected: serde_json::Value, computed: serde_json::Value) -> &'static str {
    classify(&expected, &computed)
}

#[test]
fn classify_n_mismatch_first() {
    assert_eq!(
        classify_case(
            serde_json::json!({"mean": 20.0, "sd": 10.0, "n": 3}),
            serde_json::json!({"mean": 20.0, "sd": 10.0, "n": 2, "values": [10.0, 30.0]}),
        ),
        "n_mismatch"
    );
}

#[test]
fn classify_source_sd_matches_n_divisor_signature() {
    // Real cnet GLT row: stored sd 4.99 is the n-divisor sd of the recomputed 6.1101.
    assert_eq!(
        classify_case(
            serde_json::json!({"mean": 80.33, "sd": 4.99, "n": 3}),
            serde_json::json!({"mean": 80.3333, "sd": 6.1101, "n": 3,
                               "values": [75.0, 79.0, 87.0]}),
        ),
        "source_sd_matches_n_divisor"
    );
}

#[test]
fn classify_stale_subset_signature() {
    // Real cnet VAD row: the stored 220.5 is the mean of the first two replicates only.
    assert_eq!(
        classify_case(
            serde_json::json!({"mean": 220.5, "sd": 19.5, "n": 3}),
            serde_json::json!({"mean": 291.6667, "sd": 125.9, "n": 3,
                               "values": [201.0, 240.0, 434.0]}),
        ),
        "stale_subset"
    );
}

#[test]
fn classify_stale_subset_reads_indexed_values() {
    assert_eq!(
        classify_case(
            serde_json::json!({"mean": 220.5, "sd": 19.5, "n": 3}),
            serde_json::json!({"mean": 291.6667, "sd": 125.9, "n": 3,
                               "values": [{"index": 1, "value": 201.0},
                                          {"index": 3, "value": 240.0},
                                          {"index": 4, "value": 434.0}]}),
        ),
        "stale_subset"
    );
}

#[test]
fn a_legacy_hold_has_values_without_indexes() {
    let legacy = stored_values(&serde_json::json!({"values": [1.0, 2.5]}));
    assert_eq!(legacy, vec![(None, 1.0), (None, 2.5)]);
    let indexed = stored_values(&serde_json::json!({
        "values": [{"index": 2, "value": 1.0}, {"index": 5, "value": 2.5}]
    }));
    assert_eq!(indexed, vec![(Some(2), 1.0), (Some(5), 2.5)]);
}

#[test]
fn classify_unexplained() {
    assert_eq!(
        classify_case(
            serde_json::json!({"mean": 50.0, "n": 3}),
            serde_json::json!({"mean": 49.0, "n": 3, "values": [25.0, 49.0, 73.0]}),
        ),
        "unexplained"
    );
}

#[test]
fn an_n_divisor_shaped_sd_with_a_disagreeing_mean_is_not_source_sd_matches_n_divisor() {
    assert_eq!(
        classify_case(
            serde_json::json!({"mean": 60.0, "sd": 4.99, "n": 3}),
            serde_json::json!({"mean": 80.3333, "sd": 6.1101, "n": 3,
                               "values": [75.0, 79.0, 87.0]}),
        ),
        "unexplained"
    );
}

#[test]
fn resolution_history_is_preserved() {
    let first = merged_resolution(None, serde_json::json!({"action": "accept_ours"}), "a");
    assert!(first.get("history").is_none());
    assert_eq!(first["by"], "a");
    assert!(first.get("at").is_some());
    let second = merged_resolution(Some(first), serde_json::json!({"action": "reopened"}), "x");
    assert_eq!(second["history"][0]["action"], "accept_ours");
    assert_eq!(second["by"], "x");
    let third = merged_resolution(
        Some(second),
        serde_json::json!({"action": "flag_replicates", "replicate_indexes": [2]}),
        "y",
    );
    assert_eq!(third["action"], "flag_replicates");
    assert_eq!(third["by"], "y");
    assert_eq!(third["history"][0]["action"], "accept_ours");
    assert_eq!(third["history"][1]["action"], "reopened");
    assert_eq!(third["history"][1]["by"], "x");
}

#[test]
fn changed_expectations_reopen_terminal_holds() {
    let recorded = serde_json::json!({"mean": 25.0, "sd": 10.0});
    let same = GroupAudit {
        time: chrono::Utc::now(),
        expected_mean: Some(25.0),
        expected_sd: Some(10.0),
        expected_n: None,
    };
    assert!(!expected_changed(&recorded, &same));
    let moved = GroupAudit {
        expected_mean: Some(26.0),
        ..same.clone()
    };
    assert!(expected_changed(&recorded, &moved));
    let gained_n = GroupAudit {
        expected_n: Some(3),
        ..same.clone()
    };
    assert!(expected_changed(&recorded, &gained_n));
    let lost_sd = GroupAudit {
        expected_sd: None,
        ..same
    };
    assert!(expected_changed(&recorded, &lost_sd));
}

#[test]
fn bound_sql_carries_the_quantum_floor() {
    let sql = bound_sql("a.v", "b.v", "$3", DEFAULT_ABS_TOL);
    assert!(sql.contains(&QUANTUM_FLOOR.to_string()), "{sql}");
    assert!(sql.contains(&DEFAULT_ABS_TOL.to_string()), "{sql}");
}

fn hold(key: HoldKey, status: HoldStatus) -> Hold<'static> {
    Hold {
        key,
        kind: HoldKind::ReplicateStats,
        expected: serde_json::json!({ "mean": 1.0 }),
        computed: serde_json::json!({ "mean": 2.0 }),
        delta: serde_json::json!({ "mean": -1.0 }),
        status,
        tool: None,
    }
}

fn at() -> DateTime<Utc> {
    chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 1, 1, 0, 0, 0).unwrap()
}

#[test]
fn a_stream_hold_conflicts_on_the_open_stream_index() {
    let sql = hold_statement(&hold(
        HoldKey::Stream {
            stream_id: Uuid::nil(),
            group_time: at(),
        },
        HoldStatus::Pending,
    ))
    .to_string(sea_orm::sea_query::PostgresQueryBuilder);
    assert!(
        sql.contains(
            r#"ON CONFLICT ("stream_id", "group_time", "kind") WHERE status IN ('pending', 'deferred')"#
        ),
        "{sql}"
    );
}

#[test]
fn a_slot_hold_conflicts_on_the_streamless_event_index() {
    let sql = hold_statement(&hold(
        HoldKey::Slot {
            site_id: Uuid::nil(),
            parameter_id: Uuid::nil(),
            group_time: at(),
        },
        HoldStatus::Pending,
    ))
    .to_string(sea_orm::sea_query::PostgresQueryBuilder);
    assert!(
        sql.contains(
            r#"ON CONFLICT ("kind", "site_id", "parameter_id", "group_time") WHERE stream_id IS NULL AND status = 'pending'"#
        ),
        "{sql}"
    );
    assert!(
        sql.contains(r#""site_id", "parameter_id", "group_time", "kind""#),
        "{sql}"
    );
}

#[test]
fn a_standing_stream_hold_stamps_its_own_instant_and_conflicts_on_the_stream() {
    let sql = hold_statement(&hold(
        HoldKey::StreamStanding {
            stream_id: Uuid::nil(),
        },
        HoldStatus::Pending,
    ))
    .to_string(sea_orm::sea_query::PostgresQueryBuilder);
    assert!(
        sql.contains("VALUES ('00000000-0000-0000-0000-000000000000', NOW(),"),
        "{sql}"
    );
    assert!(
        sql.contains(r#"ON CONFLICT ("stream_id") WHERE kind = 'source_identity_changed'"#),
        "{sql}"
    );
}

#[test]
fn a_re_detection_refreshes_the_payload_and_promotes_a_deferred_hold_only() {
    let sql = hold_statement(&hold(
        HoldKey::Stream {
            stream_id: Uuid::nil(),
            group_time: at(),
        },
        HoldStatus::Pending,
    ))
    .to_string(sea_orm::sea_query::PostgresQueryBuilder);
    assert!(
        sql.contains(r#""expected" = "excluded"."expected""#),
        "{sql}"
    );
    assert!(sql.contains(r#""created_at" = NOW()"#), "{sql}");
    assert!(
        sql.contains(
            r#""status" = CASE WHEN replicate_audit_holds.status = 'deferred' AND EXCLUDED.status = 'pending' THEN 'pending' ELSE replicate_audit_holds.status END"#
        ),
        "{sql}"
    );
}

#[test]
fn an_unpaired_stream_defers_its_hold() {
    assert_eq!(status_for(true), HoldStatus::Pending);
    assert_eq!(status_for(false), HoldStatus::Deferred);
}
