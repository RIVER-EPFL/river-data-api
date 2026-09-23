use super::*;
use crate::routes::private::tools::models::ManifestCurve;
use crate::routes::private::tools::models::ManifestParam;

fn slots() -> Vec<ManifestCurve> {
    vec![ManifestCurve {
        name: "std_curve".into(),
        label: "Standard curve".into(),
        required: false,
        description: None,
    }]
}

fn doc_params() -> Vec<ManifestParam> {
    serde_json::from_value(serde_json::json!([
        { "name": "DOC", "label": "DOC", "kind": "replicates",
          "parameter_code": "DOC", "curve": "std_curve" },
        { "name": "volume", "label": "Volume", "kind": "number" }
    ]))
    .unwrap()
}

const CURVE_A: Uuid = Uuid::from_u128(0xa);
const CURVE_B: Uuid = Uuid::from_u128(0xb);

#[test]
fn test_curve_column_matches_a_slot_name_case_insensitively() {
    assert_eq!(
        curve_column(&slots(), "STD_Curve").as_deref(),
        Some("std_curve")
    );
    assert_eq!(curve_column(&slots(), "DOC_rep_1"), None);
}

#[test]
fn test_row_curves_cell_overrides_the_request_default() {
    let defaults = HashMap::from([("std_curve".to_string(), CURVE_A)]);
    let curves = row_curves(&defaults, &[("std_curve", &CURVE_B.to_string())]).unwrap();
    assert_eq!(curves["std_curve"], CURVE_B);
}

#[test]
fn test_row_curves_blank_cell_keeps_the_request_default() {
    let defaults = HashMap::from([("std_curve".to_string(), CURVE_A)]);
    let curves = row_curves(&defaults, &[("std_curve", "  ")]).unwrap();
    assert_eq!(curves["std_curve"], CURVE_A);
}

#[test]
fn test_row_curves_without_a_column_or_default_names_nothing() {
    assert!(row_curves(&HashMap::new(), &[]).unwrap().is_empty());
}

#[test]
fn test_row_curves_rejects_a_cell_that_is_not_an_id() {
    let err = row_curves(&HashMap::new(), &[("std_curve", "plate 3")]).unwrap_err();
    assert!(
        err.contains("std_curve") && err.contains("plate 3"),
        "{err}"
    );
}

#[test]
fn test_replicate_curve_is_the_slot_the_param_declares() {
    let curves = HashMap::from([("std_curve".to_string(), CURVE_A)]);
    assert_eq!(
        replicate_curve(&doc_params(), "DOC", &curves),
        Some(CURVE_A)
    );
    // A param declaring no slot carries nothing, whatever the row resolved.
    assert_eq!(replicate_curve(&doc_params(), "volume", &curves), None);
    // A declared slot the row left empty carries nothing.
    assert_eq!(replicate_curve(&doc_params(), "DOC", &HashMap::new()), None);
}

#[test]
fn test_timestamp_column_accepts_the_three_header_names_in_any_case() {
    assert_eq!(timestamp_column(&["DateTime", "Depth"]), Ok(0));
    assert_eq!(
        timestamp_column(&["Site_ID", "Date", "WaterTempdegC"]),
        Ok(1)
    );
    assert_eq!(timestamp_column(&["Site_ID", "TIME"]), Ok(1));
}

#[test]
fn test_timestamp_column_names_the_headers_it_saw_when_none_is_a_time() {
    let refused = timestamp_column(&["Site_ID", "WaterTempdegC"]).unwrap_err();
    assert!(refused.contains("Site_ID"), "{refused}");
    assert!(refused.contains("WaterTempdegC"), "{refused}");
}

// --- The site a row belongs to (M202) ---

const SITE_A: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00a1);
const SITE_B: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00b2);

fn twelve_sites() -> SiteLookup {
    SiteLookup {
        by_id: HashSet::from([SITE_A, SITE_B]),
        by_name: HashMap::from([("and".to_string(), SITE_A), ("vim".to_string(), SITE_B)]),
    }
}

#[test]
fn test_site_column_is_the_declared_one_or_the_files_own() {
    let headers = ["Date", "Site_ID", "PAR1Lux_measured"];
    assert_eq!(site_column_index(&headers, None), Ok(Some(1)));
    assert_eq!(site_column_index(&headers, Some("site_id")), Ok(Some(1)));
    // A file naming no site keeps the request's single target.
    assert_eq!(site_column_index(&["Date", "Depth"], None), Ok(None));
    // A declared column the file lacks is refused, not ignored.
    assert!(site_column_index(&headers, Some("station")).is_err());
}

#[test]
fn test_a_row_resolves_its_own_site_by_name_or_id() {
    let sites = twelve_sites();
    assert_eq!(resolve_row_site("AND", &sites, SITE_B), Ok(SITE_A));
    assert_eq!(resolve_row_site(" vim ", &sites, SITE_A), Ok(SITE_B));
    assert_eq!(
        resolve_row_site(&SITE_A.to_string(), &sites, SITE_B),
        Ok(SITE_A)
    );
    // An empty cell is the request's target; an unknown name is nobody's.
    assert_eq!(resolve_row_site("", &sites, SITE_B), Ok(SITE_B));
    let err = resolve_row_site("PEU", &sites, SITE_B).unwrap_err();
    assert!(err.contains("PEU"), "{err}");
    // A well-formed id no site carries does not pass as one.
    let err = resolve_row_site(&Uuid::nil().to_string(), &sites, SITE_B).unwrap_err();
    assert!(err.contains(&Uuid::nil().to_string()), "{err}");
}

mod csv_cells {
    use super::super::{parse_datetime, replicate_column};
    use crate::routes::private::tools::models::ManifestParam;
    use chrono::{Duration, TimeZone, Utc};

    fn param(name: &str, kind: &str) -> ManifestParam {
        ManifestParam {
            name: name.to_string(),
            label: name.to_string(),
            kind: kind.to_string(),
            units: None,
            required: false,
            default: None,
            when: None,
            structure: None,
            description: None,
            section: None,
            parameter_code: None,
            suggested: None,
            curve: None,
            parameter: None,
        }
    }

    #[test]
    fn test_a_naive_timestamp_takes_the_declared_zone() {
        // 10:00 at UTC+2 is 08:00 UTC
        assert_eq!(
            parse_datetime("2025-06-01 10:00:00", Duration::hours(2)),
            Some(Utc.with_ymd_and_hms(2025, 6, 1, 8, 0, 0).unwrap())
        );
        assert_eq!(
            parse_datetime(" 2025-06-01 10:00 ", Duration::hours(2)),
            Some(Utc.with_ymd_and_hms(2025, 6, 1, 8, 0, 0).unwrap())
        );
    }

    #[test]
    fn test_an_rfc3339_timestamp_keeps_its_own_offset() {
        assert_eq!(
            parse_datetime("2025-06-01T10:00:00+02:00", Duration::hours(5)),
            Some(Utc.with_ymd_and_hms(2025, 6, 1, 8, 0, 0).unwrap())
        );
    }

    #[test]
    fn test_an_unreadable_timestamp_is_none() {
        for cell in ["", "01/06/2025", "2025-06-01", "yesterday"] {
            assert_eq!(parse_datetime(cell, Duration::zero()), None, "{cell:?}");
        }
    }

    #[test]
    fn test_a_replicate_header_names_its_param_and_position() {
        let params = [param("absorbance", "replicates"), param("volume", "number")];
        assert_eq!(
            replicate_column(&params, "absorbance_rep_1"),
            Some(("absorbance".to_string(), Some(0)))
        );
        assert_eq!(
            replicate_column(&params, "Absorbance_3"),
            Some(("absorbance".to_string(), Some(2)))
        );
    }

    #[test]
    fn test_a_header_that_is_not_a_replicate_position_is_none() {
        let params = [param("absorbance", "replicates"), param("volume", "number")];
        for header in [
            "absorbance",
            "absorbance_rep_0",
            "absorbance_0",
            "absorbance_x",
            "volume_1",
        ] {
            assert_eq!(replicate_column(&params, header), None, "{header}");
        }
    }
}

// --- A single-site import's steps ---

const DEPTH: Uuid = Uuid::from_u128(0xd1);
const TURBIDITY: Uuid = Uuid::from_u128(0xd2);
const OUTPUT: Uuid = Uuid::from_u128(0xd3);
const ELSEWHERE: Uuid = Uuid::from_u128(0xd4);
const OWNING: Uuid = Uuid::from_u128(0x51);
const API_DEPTH: Uuid = Uuid::from_u128(0x52);
const API_TURBIDITY: Uuid = Uuid::from_u128(0x53);
const SENSOR: Uuid = Uuid::from_u128(0x5e);

fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(s)
        .unwrap()
        .with_timezone(&chrono::Utc)
}

fn request(body: serde_json::Value) -> ImportCsvRequest {
    serde_json::from_value(body).unwrap()
}

fn resolver() -> ColumnResolver {
    ColumnResolver::new(
        vec![SlotColumnRow {
            parameter_id: DEPTH,
            sp_name: Some("Water depth".into()),
            param_name: Some("depth".into()),
            aliases: Some(vec!["lvl".into()]),
        }],
        vec![
            (DEPTH, "depth".into(), vec![]),
            (TURBIDITY, "turbidity".into(), vec!["ntu".into()]),
            (OUTPUT, "discharge".into(), vec![]),
            (ELSEWHERE, "Water depth".into(), vec![]),
        ],
        HashSet::from([OUTPUT]),
    )
}

fn mapping(idx: usize, header: &str, parameter_id: Uuid) -> ColumnMapping {
    ColumnMapping {
        idx,
        header: header.into(),
        parameter_id,
        conversion_factor: 1.0,
        conversion_offset: 0.0,
    }
}

fn parsed(rows: Vec<ImportRow>) -> ParsedFile {
    ParsedFile {
        earliest: rows.iter().map(|r| r.1).min(),
        latest: rows.iter().map(|r| r.1).max(),
        row_count: rows.len(),
        rows,
        errors: RowErrors::default(),
    }
}

fn overlap(owning_stream: HashMap<SlotInstant, Uuid>) -> OverlapReport {
    OverlapReport {
        identical: 0,
        differing: 0,
        sample: Vec::new(),
        owning_stream,
        identical_lines: HashSet::new(),
    }
}

#[test]
fn test_declared_offset_is_the_files_zone() {
    assert_eq!(declared_offset(Some(1.5)), chrono::Duration::minutes(90));
    assert_eq!(declared_offset(None), chrono::Duration::zero());
}

#[test]
fn test_curves_are_refused_on_a_file_naming_no_tool() {
    let curves = serde_json::json!({ "std_curve": CURVE_A });
    let plain = request(serde_json::json!({ "site": "s", "curves": curves }));
    let tool = request(serde_json::json!({ "site": "s", "curves": curves, "tool": "doc" }));
    assert!(matches!(
        require_tool_for_curves(&plain),
        Err(AppError::BadRequest(_))
    ));
    assert!(require_tool_for_curves(&tool).is_ok());
    assert!(require_tool_for_curves(&request(serde_json::json!({ "site": "s" }))).is_ok());
}

#[test]
fn test_a_header_resolves_the_sites_own_spelling_before_the_catalog() {
    let resolver = resolver();
    // "Water depth" is both the site's slot name and another parameter's catalog code.
    assert_eq!(
        resolver.resolve_header("WATER DEPTH"),
        Some((DEPTH, "Water depth".into()))
    );
    assert_eq!(
        resolver.resolve_header("lvl"),
        Some((DEPTH, "Water depth".into()))
    );
    assert_eq!(
        resolver.resolve_header("NTU"),
        Some((TURBIDITY, "turbidity".into()))
    );
    assert_eq!(resolver.resolve_header("colour"), None);
}

#[test]
fn test_a_mapping_target_resolves_by_id_or_by_name() {
    let resolver = resolver();
    assert_eq!(
        resolver.resolve_target(&TURBIDITY.to_string()),
        Some((TURBIDITY, "turbidity".into()))
    );
    assert_eq!(
        resolver.resolve_target("depth"),
        Some((DEPTH, "Water depth".into()))
    );
    assert_eq!(
        resolver.resolve_target(&Uuid::from_u128(0xff).to_string()),
        None
    );
}

#[test]
fn test_plan_columns_sorts_every_header_but_the_time() {
    let headers = [
        "DateTime",
        "depth",
        "discharge",
        "turbidity",
        "colour",
        "skipme",
        "odd",
    ];
    let explicit = HashMap::from([
        ("skipme".to_string(), None),
        ("odd".to_string(), Some("no such parameter".to_string())),
    ]);
    let plan = plan_columns(&headers, 0, Some(&explicit), &resolver(), "Martigny");

    let mapped: Vec<(usize, Uuid)> = plan
        .mappings
        .iter()
        .map(|m| (m.idx, m.parameter_id))
        .collect();
    assert_eq!(mapped, vec![(1, DEPTH), (3, TURBIDITY)]);
    assert_eq!(plan.mapped_columns["depth"], "Water depth");
    assert_eq!(plan.skipped_columns, vec!["discharge", "skipme"]);
    assert_eq!(plan.unmapped_columns, vec!["colour", "odd"]);
    assert_eq!(plan.warnings.len(), 2);
    assert!(plan.warnings[0].contains("'turbidity'") && plan.warnings[0].contains("'Martigny'"));
    assert!(plan.warnings[1].contains("'no such parameter'"));
}

#[test]
fn test_parse_rows_keeps_good_cells_and_lists_the_bad_by_line() {
    let text = "DateTime,depth\n\
                2026-01-01 00:00:00,1.5\n\
                yesterday,2.0\n\
                2026-01-01 01:00:00,\n\
                2026-01-01 02:00:00,abc\n\
                2030-01-01 00:00:00,3.0\n\
                2026-01-01 03:00:00,4.0\n";
    let mut reader = csv_reader(text);
    let headers = file_headers(&mut reader).unwrap();
    assert_eq!(headers.len(), 2);
    let file = parse_rows(
        &mut reader,
        0,
        chrono::Duration::hours(1),
        &[mapping(1, "depth", DEPTH)],
        at("2026-06-01T00:00:00Z"),
    );

    // The declared zone is UTC+1, so 00:00 local is 23:00 UTC the day before.
    assert_eq!(
        file.rows,
        vec![
            (DEPTH, at("2025-12-31T23:00:00Z"), 1.5, 2),
            (DEPTH, at("2026-01-01T02:00:00Z"), 4.0, 7),
        ]
    );
    // Every row with an admissible time counts, blank and bad cells included.
    assert_eq!(file.row_count, 4);
    assert_eq!(file.earliest, Some(at("2025-12-31T23:00:00Z")));
    assert_eq!(file.latest, Some(at("2026-01-01T02:00:00Z")));
    let lines: Vec<usize> = file.errors.listed.iter().map(|e| e.row).collect();
    assert_eq!(lines, vec![3, 5, 6]);
    assert_eq!(file.errors.count, 3);
}

#[test]
fn test_row_errors_list_is_capped_and_the_count_is_not() {
    let mut errors = RowErrors::default();
    for row in 0..MAX_ERRORS + 3 {
        errors.record(row, "bad".into());
    }
    assert_eq!(errors.listed.len(), MAX_ERRORS);
    assert_eq!(errors.count, MAX_ERRORS + 3);
}

#[test]
fn test_rows_on_a_replicate_family_slot_are_refused() {
    let t = at("2026-01-01T00:00:00Z");
    let mut file = parsed(vec![(DEPTH, t, 1.0, 2), (TURBIDITY, t, 2.0, 2)]);
    let owners = HashMap::from([((DEPTH, t), OWNING)]);
    let headers = headers_by_parameter(&[mapping(1, "depth", DEPTH)]);

    assert!(!refuse_family_slots(
        &mut file.rows,
        &owners,
        &HashMap::new(),
        &headers,
        &mut file.errors
    ));
    assert_eq!(file.rows.len(), 2);

    let families = HashMap::from([(OWNING, "cnet:DOC".to_string())]);
    assert!(refuse_family_slots(
        &mut file.rows,
        &owners,
        &families,
        &headers,
        &mut file.errors
    ));
    assert_eq!(file.rows, vec![(TURBIDITY, t, 2.0, 2)]);
    assert!(file.errors.listed[0].message.contains("Column 'depth'"));
    assert!(file.errors.listed[0].message.contains("'cnet:DOC'"));
}

#[test]
fn test_slot_cadence_resolves_most_specific_first() {
    let t = at("2026-01-01T00:00:00Z");
    let owning_stream = HashMap::from([((DEPTH, t), OWNING)]);
    let owners = HashMap::from([(
        (TURBIDITY, t),
        crate::routes::private::sensors::models::ResolvedOwner {
            sensor_id: Some(SENSOR),
            deployment_id: None,
            calibration_id: None,
        },
    )]);
    let cadence = |declared| SlotCadence {
        declared,
        owning_stream: &owning_stream,
        api_stream_of: HashMap::from([(TURBIDITY, API_TURBIDITY)]),
        stream_default: HashMap::from([(OWNING, Some("spot".to_string())), (API_TURBIDITY, None)]),
        owners: &owners,
        sensor_types: HashMap::from([(SENSOR, "spot")]),
    };

    assert_eq!(cadence(Some("continuous")).of(DEPTH, t), "continuous");
    assert_eq!(cadence(None).of(DEPTH, t), "spot");
    assert_eq!(cadence(None).of(TURBIDITY, t), "spot");
    assert_eq!(cadence(None).of(ELSEWHERE, t), "continuous");
}

#[test]
fn test_a_repeated_timestamp_is_refused_unless_the_series_is_spot() {
    let t = at("2026-01-01T00:00:00Z");
    let mut file = parsed(vec![
        (DEPTH, t, 1.0, 2),
        (DEPTH, t, 1.1, 3),
        (TURBIDITY, t, 2.0, 2),
        (TURBIDITY, t, 2.1, 3),
    ]);
    let headers = headers_by_parameter(&[mapping(1, "depth", DEPTH)]);
    let refused = refuse_repeated_slots(
        &mut file.rows,
        |pid, _| {
            (if pid == TURBIDITY {
                "spot"
            } else {
                "continuous"
            })
            .to_string()
        },
        &headers,
        &mut file.errors,
    );

    assert!(refused);
    assert_eq!(
        file.rows,
        vec![
            (DEPTH, t, 1.0, 2),
            (TURBIDITY, t, 2.0, 2),
            (TURBIDITY, t, 2.1, 3)
        ]
    );
    assert_eq!(file.errors.listed[0].row, 3);
    assert!(
        file.errors.listed[0]
            .message
            .contains("'continuous' series")
    );
}

#[test]
fn test_replicate_groups_are_counted_on_a_spot_file_only() {
    let t = at("2026-01-01T00:00:00Z");
    let rows = vec![
        (DEPTH, t, 1.0, 2),
        (DEPTH, t, 1.1, 3),
        (TURBIDITY, t, 2.0, 2),
    ];
    assert_eq!(replicate_groups(Some("spot"), &rows), 1);
    assert_eq!(replicate_groups(None, &rows), 0);
    assert_eq!(replicate_groups(Some("continuous"), &rows), 0);
}

#[test]
fn test_a_cell_already_stored_unchanged_is_not_screened() {
    let t = at("2026-01-01T00:00:00Z");
    let rows = vec![(DEPTH, t, 1.0, 2), (TURBIDITY, t, 2.0, 3)];
    assert_eq!(
        screened_cells(&rows, &HashSet::from([2])),
        vec![(3, TURBIDITY, t, 2.0)]
    );
}

#[test]
fn test_a_row_lands_on_the_stream_holding_its_slot_unless_that_stream_is_shared() {
    let t = at("2026-01-01T00:00:00Z");
    let later = at("2026-01-01T01:00:00Z");
    let api = HashMap::from([(DEPTH, API_DEPTH), (TURBIDITY, API_TURBIDITY)]);

    let alone = HashMap::from([((DEPTH, t), OWNING)]);
    let targets = WriteTargets::new(&alone, &api);
    assert_eq!(targets.of(DEPTH, t), OWNING);
    assert_eq!(targets.of(DEPTH, later), API_DEPTH);

    let shared = HashMap::from([((DEPTH, t), OWNING), ((TURBIDITY, t), OWNING)]);
    let targets = WriteTargets::new(&shared, &api);
    assert_eq!(targets.of(DEPTH, t), API_DEPTH);
    assert_eq!(targets.of(TURBIDITY, t), API_TURBIDITY);
}

#[test]
fn test_only_a_raw_file_claims_the_calibration() {
    let t = at("2026-01-01T00:00:00Z");
    let calibration = Uuid::from_u128(0xca);
    let deployment = Uuid::from_u128(0xde);
    let owners = HashMap::from([(
        (DEPTH, t),
        crate::routes::private::sensors::models::ResolvedOwner {
            sensor_id: None,
            deployment_id: Some(deployment),
            calibration_id: Some(calibration),
        },
    )]);
    let api = HashMap::from([(DEPTH, API_DEPTH)]);
    let empty = HashMap::new();
    let targets = WriteTargets::new(&empty, &api);
    let streams = HashMap::from([(
        API_DEPTH,
        TargetStream {
            slot: Some((SITE_A, DEPTH)),
            instrument: Some(SENSOR),
        },
    )]);
    let rows = vec![(DEPTH, t, 1.5, 2)];

    let raw = staged_rows(&rows, &owners, &targets, &streams, CsvValueState::Raw);
    assert_eq!(raw[0].calibration_id, Some(calibration));
    assert_eq!(raw[0].deployment_id, Some(deployment));
    // The deployment names no sensor, so the channel's instrument is stamped.
    assert_eq!(raw[0].sensor_id, Some(SENSOR));
    assert_eq!(raw[0].stream_id, API_DEPTH);
    assert_eq!(raw[0].site_id, Some(SITE_A));
    assert!((raw[0].raw_value - 1.5).abs() < f64::EPSILON);

    let corrected = staged_rows(&rows, &owners, &targets, &streams, CsvValueState::Corrected);
    assert_eq!(corrected[0].calibration_id, None);
}

#[test]
fn test_a_row_on_an_unpaired_stream_is_staged_unattributed() {
    let t = at("2026-01-01T00:00:00Z");
    let owned = crate::routes::private::sensors::models::ResolvedOwner {
        sensor_id: Some(SENSOR),
        deployment_id: Some(Uuid::from_u128(0xde)),
        calibration_id: Some(Uuid::from_u128(0xca)),
    };
    let owners = HashMap::from([((DEPTH, t), owned.clone()), ((TURBIDITY, t), owned)]);
    let api = HashMap::from([(DEPTH, API_DEPTH), (TURBIDITY, API_TURBIDITY)]);
    let empty = HashMap::new();
    let targets = WriteTargets::new(&empty, &api);
    // Turbidity has no slot at the site, so its channel is unpaired.
    let streams = HashMap::from([
        (
            API_DEPTH,
            TargetStream {
                slot: Some((SITE_A, DEPTH)),
                instrument: Some(SENSOR),
            },
        ),
        (
            API_TURBIDITY,
            TargetStream {
                slot: None,
                instrument: Some(SENSOR),
            },
        ),
    ]);
    let rows = vec![(DEPTH, t, 1.0, 2), (TURBIDITY, t, 2.0, 2)];

    let staged = staged_rows(&rows, &owners, &targets, &streams, CsvValueState::Raw);
    assert_eq!(staged[0].site_id, Some(SITE_A));
    assert_eq!(staged[0].parameter_id, Some(DEPTH));
    assert_eq!(staged[0].sensor_id, Some(SENSOR));

    let unpaired = &staged[1];
    assert_eq!(unpaired.stream_id, API_TURBIDITY);
    assert_eq!(unpaired.site_id, None);
    assert_eq!(unpaired.parameter_id, None);
    assert_eq!(unpaired.sensor_id, None);
    assert_eq!(unpaired.deployment_id, None);
    assert_eq!(unpaired.calibration_id, None);
    assert!((unpaired.raw_value - 2.0).abs() < f64::EPSILON);
}

#[test]
fn test_import_tally_by_conflict_mode() {
    // 10 rows: 3 already stored unchanged, 2 stored with another value.
    let skip = import_tally(10, 3, 2, ConflictMode::Skip);
    assert!(skip.has_work);
    assert_eq!(
        (skip.inserted_total, skip.duplicates, skip.overwritten),
        (5, 5, 0)
    );
    let overwrite = import_tally(10, 3, 2, ConflictMode::Overwrite);
    assert_eq!(
        (
            overwrite.inserted_total,
            overwrite.duplicates,
            overwrite.overwritten
        ),
        (5, 3, 2)
    );
    assert_eq!(overwrite.overlapping, 5);

    assert!(!import_tally(5, 3, 2, ConflictMode::Skip).has_work);
    assert!(import_tally(5, 3, 2, ConflictMode::Overwrite).has_work);
    assert!(!import_tally(3, 3, 0, ConflictMode::Overwrite).has_work);
}

#[test]
fn test_distinct_instants_counts_each_timestamp_once() {
    let t = at("2026-01-01T00:00:00Z");
    let later = at("2026-01-01T01:00:00Z");
    let rows = vec![
        (DEPTH, t, 1.0, 2),
        (TURBIDITY, t, 2.0, 2),
        (DEPTH, later, 1.0, 3),
    ];
    assert_eq!(distinct_instants(&rows), 2);
    assert_eq!(distinct_instants(&[]), 0);
}

#[test]
fn test_import_job_params_carry_what_the_worker_reads() {
    let t = at("2026-01-01T00:00:00Z");
    let token = Uuid::from_u128(0x70);
    let req = request(serde_json::json!({
        "site": "s", "conflict": "overwrite", "measurement_type": "spot"
    }));
    let file = parsed(vec![(DEPTH, t, 1.0, 2)]);
    let mappings = [mapping(1, "depth", DEPTH)];
    let api = HashMap::from([(DEPTH, API_DEPTH)]);

    let params = import_job_params(
        token,
        SITE_A,
        "Martigny",
        &req,
        &file,
        4,
        param_streams(&mappings, &api),
    );
    assert_eq!(
        params,
        serde_json::json!({
            "import_token": token,
            "site_id": SITE_A,
            "site_name": "Martigny",
            "conflict": "overwrite",
            "since": t.to_rfc3339(),
            "latest": t.to_rfc3339(),
            "overlapping": 4,
            "param_streams": [[DEPTH, API_DEPTH]],
            "measurement_type": "spot",
        })
    );
}

#[test]
fn test_no_resolved_column_is_refused() {
    let plan = plan_columns(&["DateTime", "colour"], 0, None, &resolver(), "Martigny");
    assert!(matches!(
        require_columns(&plan),
        Err(AppError::BadRequest(_))
    ));
}

#[test]
fn test_a_dry_run_reports_the_plan_and_writes_nothing() {
    let t = at("2026-01-01T00:00:00Z");
    let analysis = |rows| SiteAnalysis {
        site_id: SITE_A,
        site_name: "Martigny".into(),
        session_id: Uuid::from_u128(0x5e55),
        plan: plan_columns(&["DateTime", "depth"], 0, None, &resolver(), "Martigny"),
        parsed: parsed(rows),
        replicate_groups: 0,
        overlap: OverlapReport {
            identical: 1,
            differing: 1,
            ..overlap(HashMap::new())
        },
        check: None,
    };
    let rows = vec![
        (DEPTH, t, 1.0, 2),
        (DEPTH, at("2026-01-01T01:00:00Z"), 2.0, 3),
    ];

    let preview = analysis(rows.clone()).response(None);
    assert!(preview.dry_run);
    assert_eq!(preview.row_count, 2);
    assert_eq!(preview.overlaps_identical, 1);
    assert_eq!(preview.overlaps_differing, 1);
    assert_eq!(
        (
            preview.inserted_total,
            preview.duplicates,
            preview.overwritten
        ),
        (0, 0, 0)
    );
    assert_eq!(preview.derived_job_id, None);

    let job = Uuid::from_u128(0x10b);
    let done = analysis(rows).response(Some(Committed {
        tally: import_tally(2, 1, 1, ConflictMode::Overwrite),
        derived_job_id: Some(job),
        derived_timestamps: 2,
    }));
    assert!(!done.dry_run);
    assert_eq!(
        (done.inserted_total, done.duplicates, done.overwritten),
        (0, 1, 1)
    );
    assert_eq!(done.derived_job_id, Some(job));
    assert_eq!(done.derived_timestamps, 2);
    assert_eq!(done.session_id, Some(Uuid::from_u128(0x5e55)));
}
