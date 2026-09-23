use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, TimeZone, Utc};
use uuid::Uuid;

use super::admission::RejectionKind;
use super::{
    Cache, ClaimedCurves, DiffOutcome, IngestAttribution, IngestCurves, IngestEffect, IngestFunnel,
    Refresh, advance_cursor, classified_outcome, clean_digest, derived_timestamps,
    is_nothing_to_ingest, pass_touched_groups, reading_model, refuse_sync_only_claims,
    require_spot_window, rows_to_write, spot_window, strip_inadmissible_curve_claims,
};
use crate::error::AppError;
use crate::routes::private::readings::models::{
    IngestReading, IngestReadingsRequest, SourceWindow,
};
use crate::routes::private::sensor_calibrations::service::Curve;
use crate::routes::private::sensors::models::{ResolvedOwner, ResolvedSlot};

fn at(minute: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap() + Duration::minutes(minute)
}

fn reading(minute: i64, value: f64) -> IngestReading {
    IngestReading::new(at(minute), value)
}

fn replicate(minute: i64, index: i16, value: f64) -> IngestReading {
    IngestReading {
        replicate_index: index,
        ..reading(minute, value)
    }
}

fn window() -> SourceWindow {
    SourceWindow {
        from: at(0),
        to: at(60),
        source_rows_read: 1,
        dropped_times: Vec::new(),
        content_digest: Some("digest".to_string()),
    }
}

fn request(readings: Vec<IngestReading>) -> IngestReadingsRequest {
    IngestReadingsRequest {
        stream_id: Uuid::nil(),
        readings,
        overwrite: false,
        collection: false,
        audit: None,
        window: None,
    }
}

fn attribution() -> IngestAttribution {
    IngestAttribution {
        stream_sensor: None,
        stream_measurement_type: None,
        site_id: Some(Uuid::new_v4()),
        parameter_id: Some(Uuid::new_v4()),
        windows: HashMap::new(),
        owners: HashMap::new(),
        sensor_types: HashMap::new(),
    }
}

fn curve(slope: f64, intercept: f64) -> Curve {
    Curve {
        id: Uuid::new_v4(),
        slope,
        intercept,
    }
}

fn keys(readings: &[IngestReading]) -> Vec<(DateTime<Utc>, i16, f64)> {
    readings
        .iter()
        .map(|r| (r.time, r.replicate_index, r.raw_value))
        .collect()
}

// --- Refusals before anything is read ---

#[test]
fn test_refuse_sync_only_claims_from_another_caller() {
    let overwrite = IngestReadingsRequest {
        overwrite: true,
        ..request(vec![reading(0, 1.0)])
    };
    let collection = IngestReadingsRequest {
        collection: true,
        ..request(vec![reading(0, 1.0)])
    };
    let audit = IngestReadingsRequest {
        audit: Some(Vec::new()),
        ..request(vec![reading(0, 1.0)])
    };
    let windowed = IngestReadingsRequest {
        window: Some(window()),
        ..request(vec![reading(0, 1.0)])
    };
    for payload in [&overwrite, &collection, &audit, &windowed] {
        assert!(matches!(
            refuse_sync_only_claims(payload, false),
            Err(AppError::Forbidden(_))
        ));
        assert!(refuse_sync_only_claims(payload, true).is_ok());
    }
    assert!(refuse_sync_only_claims(&request(vec![reading(0, 1.0)]), false).is_ok());
}

#[test]
fn test_refuse_sync_only_claims_window_closing_on_itself() {
    let payload = IngestReadingsRequest {
        window: Some(SourceWindow {
            from: at(60),
            to: at(60),
            ..window()
        }),
        ..request(Vec::new())
    };
    assert!(matches!(
        refuse_sync_only_claims(&payload, true),
        Err(AppError::BadRequest(_))
    ));
}

#[test]
fn test_is_nothing_to_ingest_empty_without_claim() {
    assert!(is_nothing_to_ingest(&request(Vec::new())));
    assert!(!is_nothing_to_ingest(&request(vec![reading(0, 1.0)])));
    let claim = IngestReadingsRequest {
        window: Some(window()),
        ..request(Vec::new())
    };
    assert!(
        !is_nothing_to_ingest(&claim),
        "an empty window is a claim the source holds nothing there"
    );
}

#[test]
fn test_require_spot_window_only_on_spot_stream() {
    let w = window();
    assert!(require_spot_window(None, None).is_ok());
    assert!(require_spot_window(Some(&w), Some("spot")).is_ok());
    assert!(matches!(
        require_spot_window(Some(&w), Some("continuous")),
        Err(AppError::BadRequest(_))
    ));
    assert!(matches!(
        require_spot_window(Some(&w), None),
        Err(AppError::BadRequest(_))
    ));
}

// --- The admission funnel ---

#[test]
fn test_funnel_admit_counts_by_kind_and_keeps_survivors() {
    let mut readings = vec![
        reading(0, 1.0),
        reading(1, f64::NAN),
        reading(2, f64::INFINITY),
        reading(3, 4.0),
    ];
    let mut funnel = IngestFunnel::new(&readings, false);
    funnel.admit(&mut readings, at(10));
    assert_eq!(keys(&readings), vec![(at(0), 0, 1.0), (at(3), 0, 4.0)]);
    assert_eq!(funnel.rejected_total(), 2);
    assert_eq!(
        funnel.rejected_by_kind(),
        serde_json::json!({ RejectionKind::NonFinite.as_str(): 2 })
    );
    let outcome = funnel.outcome(Uuid::nil(), true, 0);
    assert_eq!(outcome.skipped, 2);
    assert!(outcome.paired);
}

#[test]
fn test_funnel_remembers_refused_keys_only_under_a_window() {
    let mut readings = vec![reading(0, f64::NAN)];
    let mut appending = IngestFunnel::new(&readings, false);
    appending.admit(&mut readings.clone(), at(10));
    assert!(appending.rejected_keys().is_empty());

    let mut windowed = IngestFunnel::new(&readings, true);
    windowed.admit(&mut readings, at(10));
    assert_eq!(
        windowed.rejected_keys(),
        &HashSet::from([(at(0), 0)]),
        "a refused key is retained by the diff, not withdrawn"
    );
}

#[test]
fn test_funnel_keep_last_per_key_under_a_window() {
    let original = vec![reading(0, 1.0), reading(1, 2.0), reading(0, 3.0)];

    let mut appending = original.clone();
    let mut funnel = IngestFunnel::new(&appending, false);
    funnel.keep_last_per_key(&mut appending);
    assert_eq!(appending.len(), 3, "an append keeps every row");

    let mut windowed = original;
    let mut funnel = IngestFunnel::new(&windowed, true);
    funnel.keep_last_per_key(&mut windowed);
    assert_eq!(keys(&windowed), vec![(at(1), 0, 2.0), (at(0), 0, 3.0)]);
    assert_eq!(
        funnel.rejected_by_kind(),
        serde_json::json!({ RejectionKind::DuplicateKey.as_str(): 1 })
    );
    assert!(
        funnel.rejected_keys().is_empty(),
        "the key is still carried by the last occurrence"
    );
}

#[test]
fn test_funnel_drop_unknown_calibrations() {
    let known = curve(2.0, 0.0);
    let mut readings = vec![
        IngestReading {
            calibration_id: Some(known.id),
            ..reading(0, 1.0)
        },
        IngestReading {
            calibration_id: Some(Uuid::new_v4()),
            ..reading(1, 1.0)
        },
        reading(2, 1.0),
    ];
    let declared = HashMap::from([(known.id, known)]);
    let mut funnel = IngestFunnel::new(&readings, true);
    funnel.drop_unknown_calibrations(&mut readings, &declared);
    assert_eq!(keys(&readings), vec![(at(0), 0, 1.0), (at(2), 0, 1.0)]);
    assert_eq!(funnel.rejected_keys(), &HashSet::from([(at(1), 0)]));
}

#[test]
fn test_funnel_drop_replicates_off_spot() {
    let spot_sensor = Uuid::new_v4();
    let mut attribution = attribution();
    attribution.sensor_types = HashMap::from([(spot_sensor, "spot")]);
    let mut readings = vec![
        reading(0, 1.0),
        replicate(0, 1, 2.0),
        IngestReading {
            sensor_id: Some(spot_sensor),
            ..replicate(1, 1, 3.0)
        },
        IngestReading {
            measurement_type: Some("spot".to_string()),
            ..replicate(2, 2, 4.0)
        },
    ];
    let mut funnel = IngestFunnel::new(&readings, false);
    funnel.drop_replicates_off_spot(&mut readings, &attribution);
    assert_eq!(
        keys(&readings),
        vec![(at(0), 0, 1.0), (at(1), 1, 3.0), (at(2), 2, 4.0)],
        "a replicate is kept where the cadence resolves to spot, by row, stream or instrument"
    );
    assert_eq!(
        funnel.rejected_by_kind(),
        serde_json::json!({ RejectionKind::ReplicateIndexOnNonSpot.as_str(): 1 })
    );
}

// --- Attribution ---

#[test]
fn test_attribution_instrument_precedence() {
    let row = Uuid::new_v4();
    let stream = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let mut attribution = attribution();
    attribution.owners = HashMap::from([(
        at(0),
        ResolvedOwner {
            sensor_id: Some(owner),
            ..ResolvedOwner::default()
        },
    )]);
    assert_eq!(attribution.instrument_of(&reading(0, 1.0)), Some(owner));
    attribution.stream_sensor = Some(stream);
    assert_eq!(attribution.instrument_of(&reading(0, 1.0)), Some(stream));
    let named = IngestReading {
        sensor_id: Some(row),
        ..reading(0, 1.0)
    };
    assert_eq!(attribution.instrument_of(&named), Some(row));
    assert_eq!(attribution.instrument_of(&reading(5, 1.0)), Some(stream));
}

#[test]
fn test_attribution_candidate_instruments_sorted_once() {
    let a = Uuid::from_u128(1);
    let b = Uuid::from_u128(2);
    let mut attribution = attribution();
    attribution.stream_sensor = Some(b);
    attribution.owners = HashMap::from([(
        at(0),
        ResolvedOwner {
            sensor_id: Some(a),
            ..ResolvedOwner::default()
        },
    )]);
    let readings = vec![
        IngestReading {
            sensor_id: Some(b),
            ..reading(0, 1.0)
        },
        IngestReading {
            sensor_id: Some(a),
            ..reading(1, 1.0)
        },
    ];
    assert_eq!(attribution.candidate_instruments(&readings), vec![a, b]);
}

#[test]
fn test_attribution_calibration_requests_skip_uninstrumented() {
    let sensor = Uuid::new_v4();
    let attribution = attribution();
    let readings = vec![
        IngestReading {
            sensor_id: Some(sensor),
            ..reading(0, 1.0)
        },
        reading(1, 1.0),
    ];
    assert_eq!(
        attribution.calibration_requests(&readings),
        vec![(sensor, attribution.parameter_id, at(0))]
    );
}

// --- Standard curve claims ---

#[test]
fn test_strip_inadmissible_curve_claims() {
    let instrument = Uuid::new_v4();
    let elsewhere = Uuid::new_v4();
    let homed = curve(2.0, 0.0);
    let foreign = curve(3.0, 0.0);
    let claimed = ClaimedCurves {
        instruments: HashMap::from([(homed.id, instrument), (foreign.id, elsewhere)]),
        curves: HashMap::from([(homed.id, homed), (foreign.id, foreign)]),
    };
    let mut attribution = attribution();
    attribution.stream_sensor = Some(instrument);
    attribution.stream_measurement_type = Some("spot".to_string());
    let missing = Uuid::new_v4();
    let mut readings = vec![
        IngestReading {
            standard_curve_id: Some(homed.id),
            ..reading(0, 1.0)
        },
        IngestReading {
            standard_curve_id: Some(foreign.id),
            ..reading(1, 1.0)
        },
        IngestReading {
            standard_curve_id: Some(missing),
            ..reading(2, 1.0)
        },
        IngestReading {
            standard_curve_id: Some(homed.id),
            measurement_type: Some("continuous".to_string()),
            ..reading(3, 1.0)
        },
    ];
    let stripped = strip_inadmissible_curve_claims(&mut readings, &claimed, &attribution);
    assert_eq!(
        readings.len(),
        4,
        "a stripped claim never costs the reading"
    );
    assert_eq!(readings[0].standard_curve_id, Some(homed.id));
    assert!(readings[1..].iter().all(|r| r.standard_curve_id.is_none()));
    let reason = |minute| {
        stripped[&at(minute)][0]["reason"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        reason(1),
        "fitted on a different instrument than the reading's"
    );
    assert_eq!(reason(2), "names no standard curve");
    assert_eq!(reason(3), "the reading is not a spot measurement");
    assert!(!stripped.contains_key(&at(0)));
    assert_eq!(
        stripped[&at(1)][0]["curve_instrument_id"],
        serde_json::json!(elsewhere)
    );
    assert_eq!(
        stripped[&at(2)][0]["curve_instrument_id"],
        serde_json::Value::Null
    );
}

// --- The stored row ---

#[test]
fn test_reading_model_paired_composes_base_then_standard() {
    let sensor = Uuid::new_v4();
    let deployment = Uuid::new_v4();
    let moved_to = Uuid::new_v4();
    let base = curve(2.0, 1.0);
    let standard = curve(10.0, 0.0);
    let mut attribution = attribution();
    attribution.stream_sensor = Some(sensor);
    attribution.stream_measurement_type = Some("spot".to_string());
    attribution.windows = HashMap::from([(
        at(0),
        ResolvedSlot {
            calibration_id: None,
            deployment_id: Some(deployment),
            site_id: Some(moved_to),
        },
    )]);
    let curves = IngestCurves {
        resolved: HashMap::from([((sensor, attribution.parameter_id, at(0)), base)]),
        declared: HashMap::new(),
        standard: HashMap::from([(standard.id, standard)]),
    };
    let r = IngestReading {
        standard_curve_id: Some(standard.id),
        ..reading(0, 3.0)
    };
    let m = reading_model(Uuid::nil(), &r, &attribution, &curves);
    // (3 * 2 + 1) * 10
    assert_eq!(m.calibrated_value.unwrap(), Some(70.0));
    assert_eq!(m.calibration_id.unwrap(), Some(base.id));
    assert_eq!(m.standard_curve_id.unwrap(), Some(standard.id));
    assert_eq!(m.sensor_id.unwrap(), Some(sensor));
    assert_eq!(m.deployment_id.unwrap(), Some(deployment));
    assert_eq!(
        m.site_id.unwrap(),
        Some(moved_to),
        "the deployment decides the site"
    );
    assert_eq!(m.measurement_type.unwrap().as_deref(), Some("spot"));
    assert_eq!(m.provenance_kind.unwrap().as_deref(), Some("sync"));
}

#[test]
fn test_reading_model_declared_calibration_wins() {
    let sensor = Uuid::new_v4();
    let covering = curve(2.0, 0.0);
    let declared = curve(5.0, 0.0);
    let mut attribution = attribution();
    attribution.stream_sensor = Some(sensor);
    let curves = IngestCurves {
        resolved: HashMap::from([((sensor, attribution.parameter_id, at(0)), covering)]),
        declared: HashMap::from([(declared.id, declared)]),
        standard: HashMap::new(),
    };
    let r = IngestReading {
        calibration_id: Some(declared.id),
        ..reading(0, 3.0)
    };
    let m = reading_model(Uuid::nil(), &r, &attribution, &curves);
    assert_eq!(m.calibrated_value.unwrap(), Some(15.0));
    assert_eq!(m.calibration_id.unwrap(), Some(declared.id));
    assert_eq!(
        m.site_id.unwrap(),
        attribution.site_id,
        "no deployment window: the pairing's site"
    );
}

#[test]
fn test_reading_model_uncorrected_stays_null() {
    let attribution = attribution();
    let curves = IngestCurves {
        resolved: HashMap::new(),
        declared: HashMap::new(),
        standard: HashMap::new(),
    };
    let m = reading_model(Uuid::nil(), &reading(0, 3.0), &attribution, &curves);
    assert_eq!(m.calibrated_value.unwrap(), None);
    assert_eq!(m.calibration_id.unwrap(), None);
    assert_eq!(m.measurement_type.unwrap().as_deref(), Some("continuous"));
}

#[test]
fn test_reading_model_unpaired_keeps_only_what_the_caller_claimed() {
    let stream_sensor = Uuid::new_v4();
    let covering = curve(2.0, 0.0);
    let claimed_calibration = Uuid::new_v4();
    let claimed_deployment = Uuid::new_v4();
    let attribution = IngestAttribution {
        stream_sensor: Some(stream_sensor),
        site_id: None,
        parameter_id: None,
        ..attribution()
    };
    let curves = IngestCurves {
        resolved: HashMap::from([((stream_sensor, None, at(0)), covering)]),
        declared: HashMap::new(),
        standard: HashMap::new(),
    };
    let staged = reading_model(Uuid::nil(), &reading(0, 3.0), &attribution, &curves);
    assert_eq!(staged.site_id.unwrap(), None);
    assert_eq!(
        staged.sensor_id.unwrap(),
        None,
        "the channel default is not a claim"
    );
    assert_eq!(staged.calibration_id.unwrap(), None);
    assert_eq!(staged.calibrated_value.unwrap(), None);

    let claimed = IngestReading {
        calibration_id: Some(claimed_calibration),
        deployment_id: Some(claimed_deployment),
        ..reading(0, 3.0)
    };
    let staged = reading_model(Uuid::nil(), &claimed, &attribution, &curves);
    assert_eq!(staged.calibration_id.unwrap(), Some(claimed_calibration));
    assert_eq!(staged.deployment_id.unwrap(), Some(claimed_deployment));
}

#[test]
fn test_spot_window_paired_spot_rows_only() {
    let attribution = attribution();
    let curves = IngestCurves {
        resolved: HashMap::new(),
        declared: HashMap::new(),
        standard: HashMap::new(),
    };
    let spot = |minute| IngestReading {
        measurement_type: Some("spot".to_string()),
        ..reading(minute, 1.0)
    };
    let models: Vec<_> = [spot(5), reading(0, 1.0), spot(20), reading(40, 1.0)]
        .iter()
        .map(|r| reading_model(Uuid::nil(), r, &attribution, &curves))
        .collect();
    assert_eq!(spot_window(&models, true), Some((at(5), at(20))));
    assert_eq!(
        spot_window(&models, false),
        None,
        "the pairing backfill materialises"
    );
    assert_eq!(spot_window(&models[1..2], true), None);
}

// --- The write ---

#[test]
fn test_rows_to_write_under_a_diff_only_new_keys() {
    let readings = vec![reading(0, 1.0), reading(1, 2.0)];
    let attribution = attribution();
    let curves = IngestCurves {
        resolved: HashMap::new(),
        declared: HashMap::new(),
        standard: HashMap::new(),
    };
    let models: Vec<_> = readings
        .iter()
        .map(|r| reading_model(Uuid::nil(), r, &attribution, &curves))
        .collect();
    let diff = DiffOutcome {
        write_keys: HashSet::from([(at(1), 0)]),
        ..DiffOutcome::default()
    };
    assert_eq!(rows_to_write(&models, &readings, None, false).len(), 2);
    let new_only = rows_to_write(&models, &readings, Some(&diff), false);
    assert_eq!(new_only.len(), 1);
    assert_eq!(new_only[0].raw_value.clone().unwrap(), 2.0);
    assert_eq!(
        rows_to_write(&models, &readings, Some(&diff), true).len(),
        2,
        "an overwrite rewrites attribution, which value equality cannot see"
    );
}

#[test]
fn test_pass_touched_groups() {
    assert!(pass_touched_groups(false, None));
    assert!(!pass_touched_groups(false, Some(&DiffOutcome::default())));
    assert!(pass_touched_groups(true, Some(&DiffOutcome::default())));
    let withdrew = DiffOutcome {
        withdrawn: 1,
        ..DiffOutcome::default()
    };
    assert!(pass_touched_groups(false, Some(&withdrew)));
    let reinstated = DiffOutcome {
        reinstated: 1,
        ..DiffOutcome::default()
    };
    assert!(pass_touched_groups(false, Some(&reinstated)));
}

// --- After the commit ---

#[test]
fn test_effect_plain_append() {
    let payload = request(vec![reading(0, 1.0), reading(9, 1.0)]);
    let effect = IngestEffect::of(&payload, None, 2, None);
    assert_eq!(effect.moved(), 2);
    assert_eq!(effect.span, Some((at(0), at(9))));
    let axes = effect.axes();
    assert_eq!(axes.cache, Cache::Sites);
    assert_eq!(axes.refresh, Refresh::Skip);
}

#[test]
fn test_effect_correction_refreshes_and_invalidates_everything() {
    let payload = IngestReadingsRequest {
        overwrite: true,
        ..request(vec![reading(0, 1.0)])
    };
    let effect = IngestEffect::of(&payload, None, 1, None);
    assert!(effect.corrected);
    let axes = effect.axes();
    assert_eq!(axes.cache, Cache::All);
    assert_eq!(axes.refresh, Refresh::Range { fatal: false });

    let unchanged = IngestEffect::of(&payload, None, 0, None);
    assert!(
        !unchanged.corrected,
        "an overwrite that wrote nothing corrected nothing"
    );
}

#[test]
fn test_effect_withdrawal_moves_served_history() {
    let payload = IngestReadingsRequest {
        window: Some(window()),
        ..request(Vec::new())
    };
    let diff = DiffOutcome {
        withdrawn: 3,
        reinstated: 1,
        ..DiffOutcome::default()
    };
    let effect = IngestEffect::of(&payload, None, 0, Some(&diff));
    assert_eq!(effect.moved(), 4);
    assert_eq!(effect.span, None);
    assert_eq!(effect.axes().cache, Cache::All);
    let sampled = IngestEffect::of(
        &request(vec![reading(0, 1.0)]),
        Some((at(0), at(0))),
        1,
        None,
    );
    assert_eq!(
        sampled.axes().cache,
        Cache::All,
        "sample formation rewrites a served point"
    );
}

#[test]
fn test_advance_cursor_only_forward() {
    let readings = vec![reading(3, 1.0), reading(7, 1.0)];
    assert_eq!(advance_cursor(&readings, None), Some(at(7)));
    assert_eq!(advance_cursor(&readings, Some(at(5).into())), Some(at(7)));
    assert_eq!(advance_cursor(&readings, Some(at(7).into())), None);
    assert_eq!(advance_cursor(&[], None), None);
}

#[test]
fn test_clean_digest_only_for_a_cleanly_applied_window() {
    let w = window();
    let clean = DiffOutcome::default();
    assert_eq!(
        clean_digest(Some(&w), 0, true, Some(&clean)).as_deref(),
        Some("digest")
    );
    assert_eq!(clean_digest(None, 0, true, Some(&clean)), None);
    assert_eq!(clean_digest(Some(&w), 1, true, Some(&clean)), None);
    assert_eq!(clean_digest(Some(&w), 0, false, Some(&clean)), None);
    assert_eq!(clean_digest(Some(&w), 0, true, None), None);
    for held in [
        DiffOutcome {
            braked: true,
            ..DiffOutcome::default()
        },
        DiffOutcome {
            holds_raised: 1,
            ..DiffOutcome::default()
        },
        DiffOutcome {
            proposals_awaiting: 1,
            ..DiffOutcome::default()
        },
    ] {
        assert_eq!(clean_digest(Some(&w), 0, true, Some(&held)), None);
    }
}

#[test]
fn test_derived_timestamps_union_withdrawn_and_reinstated() {
    let readings = vec![reading(5, 1.0), reading(1, 1.0), replicate(5, 1, 1.0)];
    let diff = DiffOutcome {
        withdrawn_keys: vec![(at(3), 0), (at(1), 2)],
        reinstated_keys: vec![(at(9), 0)],
        ..DiffOutcome::default()
    };
    assert_eq!(derived_timestamps(&readings, None), vec![at(1), at(5)]);
    assert_eq!(
        derived_timestamps(&readings, Some(&diff)),
        vec![at(1), at(3), at(5), at(9)]
    );
}

#[test]
fn test_classified_outcome_takes_the_diff_s_account() {
    let readings = vec![reading(0, 1.0)];
    let funnel = IngestFunnel::new(&readings, true);
    let w = window();
    let plain = classified_outcome(funnel.outcome(Uuid::nil(), true, 5), None, None);
    assert_eq!(plain.inserted, 5);
    assert!(plain.accepted_window.is_none());

    let diff = DiffOutcome {
        new_rows: 1,
        changed: 2,
        proposed: 2,
        withdrawn: 3,
        unchanged: 4,
        retained: 5,
        ..DiffOutcome::default()
    };
    let windowed = classified_outcome(funnel.outcome(Uuid::nil(), true, 5), Some(&diff), Some(&w));
    assert_eq!(
        (
            windowed.inserted,
            windowed.changed,
            windowed.proposed,
            windowed.withdrawn,
            windowed.unchanged,
            windowed.retained
        ),
        (1, 2, 2, 3, 4, 5)
    );
    assert_eq!(windowed.accepted_window.map(|a| a.to), Some(w.to));
}
