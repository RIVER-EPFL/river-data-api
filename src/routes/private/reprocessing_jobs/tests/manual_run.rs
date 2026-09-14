//! What each kind answers when a person asks to run it off its cadence.
//!
//! The table is the one source the route's refusal and the page's form both read, so what it
//! answers for a kind is the contract; these pin the three classes and the refusal's wording.

use std::collections::HashMap;

use super::{ManualRun, ParamKind, build_registry, manual_run_for, missing_params, runnable_jobs};

fn declared(kind: &str) -> Vec<super::ParamSpec> {
    match manual_run_for(kind) {
        ManualRun::Declared { params } => params,
        other => panic!("{kind} is {other:?}, not a declared kind"),
    }
}

#[test]
fn a_kind_reading_nothing_from_its_params_is_a_button() {
    for kind in [
        "alarm_sweep",
        "janitor_service",
        "reprocess_all",
        "backfill_attribution",
        "event_audit",
    ] {
        assert_eq!(manual_run_for(kind), ManualRun::NoParameters, "{kind}");
    }
}

#[test]
fn a_kind_whose_inputs_are_not_a_persons_is_not_offered() {
    // csv_import consumes staged rows it then deletes; the derived trio replays persisted
    // timestamps; event_recompute replays one named visit.
    for kind in [
        "csv_import",
        "compute_derived",
        "batch_derived",
        "ingest_derived",
        "event_recompute",
    ] {
        assert_eq!(manual_run_for(kind), ManualRun::NotOffered, "{kind}");
    }
}

#[test]
fn an_unknown_name_is_not_offered() {
    assert_eq!(manual_run_for("no_such_job"), ManualRun::NotOffered);
}

#[test]
fn a_declared_kind_names_each_input_and_whether_it_is_required() {
    let reprocess = declared("manual_reprocess");
    assert_eq!(reprocess.len(), 1);
    assert_eq!(reprocess[0].name, "sensor_id");
    assert_eq!(reprocess[0].kind, ParamKind::Uuid);
    assert!(reprocess[0].required);

    let slot = declared("sensor_swap");
    assert_eq!(
        slot.iter()
            .filter(|p| p.required)
            .map(|p| p.name)
            .collect::<Vec<_>>(),
        ["site_id", "parameter_id"],
        "the slot is required and the instrument is not"
    );

    // Every one of the refresh's inputs is optional on its own: a window is either end or since.
    assert!(declared("refresh_aggregates").iter().all(|p| !p.required));
}

#[test]
fn the_refusal_names_the_missing_inputs_by_their_labels() {
    let offer = manual_run_for("merge_parameters");
    assert_eq!(
        missing_params(&offer, &serde_json::json!({})),
        ["Absorbed parameter", "Surviving parameter"]
    );
    assert_eq!(
        missing_params(
            &offer,
            &serde_json::json!({ "source_parameter_id": "00000000-0000-0000-0000-000000000001" })
        ),
        ["Surviving parameter"]
    );
    // An explicit null is as missing as an absent key.
    assert_eq!(
        missing_params(&offer, &serde_json::json!({ "target_parameter_id": null })).len(),
        2
    );
}

#[test]
fn nothing_is_missing_from_a_kind_that_declares_nothing() {
    assert!(missing_params(&ManualRun::NoParameters, &serde_json::json!({})).is_empty());
    assert!(missing_params(&ManualRun::NotOffered, &serde_json::json!({})).is_empty());
}

/// Every on-demand kind the registry carries has an answer, so the page never meets a kind it
/// cannot classify and the route never falls through to a default that lets a doomed run through.
#[test]
fn every_registered_kind_answers() {
    let registry = build_registry();
    for name in registry.names() {
        let job = registry.get(name).expect("a registered name resolves");
        let offer = job.manual_run();
        assert_eq!(
            offer,
            manual_run_for(name),
            "{name} disagrees with the table"
        );
        if let ManualRun::Declared { params } = offer {
            assert!(!params.is_empty(), "{name} declares an empty parameter set");
        }
    }
}

fn cadence(rows: &[(&str, Option<i64>, bool)]) -> HashMap<String, (Option<i64>, bool)> {
    rows.iter()
        .map(|(name, interval, enabled)| ((*name).to_string(), (*interval, *enabled)))
        .collect()
}

#[test]
fn a_kind_reached_only_from_its_own_route_is_not_listed() {
    let listed = runnable_jobs(
        ["csv_import", "alarm_sweep", "ingest_derived"].into_iter(),
        &cadence(&[]),
    );
    let names: Vec<&str> = listed.iter().map(|j| j.job_name.as_str()).collect();
    assert_eq!(names, vec!["alarm_sweep"]);
}

#[test]
fn a_kind_with_no_schedule_row_is_listed_with_no_cadence() {
    let listed = runnable_jobs(["reprocess_all"].into_iter(), &cadence(&[]));
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].interval_seconds, None, "on demand");
    assert_eq!(listed[0].enabled, None);
    assert_eq!(listed[0].manual_run, ManualRun::NoParameters);
}

#[test]
fn a_scheduled_kind_carries_its_cadence_and_whether_it_is_on() {
    let listed = runnable_jobs(
        ["alarm_sweep"].into_iter(),
        &cadence(&[("alarm_sweep", Some(60), false)]),
    );
    assert_eq!(listed[0].interval_seconds, Some(60));
    assert_eq!(listed[0].enabled, Some(false));
}

#[test]
fn the_listing_is_ordered_by_name() {
    let listed = runnable_jobs(
        ["reprocess_all", "alarm_sweep", "event_audit"].into_iter(),
        &cadence(&[]),
    );
    let names: Vec<&str> = listed.iter().map(|j| j.job_name.as_str()).collect();
    assert_eq!(names, vec!["alarm_sweep", "event_audit", "reprocess_all"]);
}

/// A declared kind reaches the page with the inputs its form is built from.
#[test]
fn a_declared_kind_carries_its_specs() {
    let listed = runnable_jobs(["measurement_retag"].into_iter(), &cadence(&[]));
    let ManualRun::Declared { params } = &listed[0].manual_run else {
        panic!("measurement_retag declares inputs");
    };
    assert!(!params.is_empty());
}
