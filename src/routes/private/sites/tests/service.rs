use super::value_source;

/// Expected behaviour: a spot period is summarised at the served instant value, a continuous one
/// at the reading. Reading the wrong side would summarise replicates as if each were a
/// measurement of its own.
#[test]
fn each_cadence_is_summarised_over_what_the_api_serves_for_it() {
    use sea_orm::sea_query::PostgresQueryBuilder;

    let site = uuid::Uuid::from_u128(1);
    let parameters = [uuid::Uuid::from_u128(2)];
    let rendered =
        |kind: &str| value_source(kind, site, &parameters).to_string(PostgresQueryBuilder);

    let spot = rendered("spot");
    assert!(spot.contains("smp.mean"), "the spot arm reads the mean");
    assert!(
        spot.contains(r#""withdrawn_at" IS NULL"#),
        "a retracted replicate is not in the period: {spot}"
    );

    let continuous = rendered("continuous");
    assert!(
        continuous.contains(r#""r"."replicate_index" = 0"#),
        "continuous rows live at index 0: {continuous}"
    );
    assert!(
        !continuous.contains("samples"),
        "a continuous reading has no sample to average: {continuous}"
    );
}

/// Expected behaviour: an instant is counted once and only when every replicate in it is
/// withdrawn, and the time bounds sit on the inner query, where chunk exclusion can see them.
/// Counting rows instead of instants would report a three-replicate retraction as three.
#[test]
fn a_withdrawn_instant_is_counted_once_and_the_window_stays_on_the_inner_query() {
    use sea_orm::sea_query::PostgresQueryBuilder;

    let site = uuid::Uuid::from_u128(1);
    let parameter = uuid::Uuid::from_u128(2);
    let start = chrono::DateTime::from_timestamp(0, 0).expect("epoch is a timestamp");

    let open = super::withdrawn_instants_query(site, &[parameter], start, None)
        .to_string(PostgresQueryBuilder);
    assert!(
        open.contains(r#"GROUP BY "parameter_id", "time""#),
        "the inner query groups by instant: {open}"
    );
    assert!(
        open.contains("HAVING bool_and(withdrawn_at IS NOT NULL)"),
        "only a wholly withdrawn instant survives: {open}"
    );
    assert!(
        open.contains(r#"FROM (SELECT"#) && open.trim_end().ends_with(r#"GROUP BY "parameter_id""#),
        "the outer count is over instants: {open}"
    );
    assert_eq!(
        open.matches(r#""time" >= "#).count(),
        1,
        "the lower bound is on the inner query only: {open}"
    );

    let bounded = super::withdrawn_instants_query(
        site,
        &[parameter],
        start,
        Some(chrono::DateTime::from_timestamp(86_400, 0).expect("a day later is a timestamp")),
    )
    .to_string(PostgresQueryBuilder);
    assert_eq!(
        bounded.matches(r#""time" <= "#).count(),
        1,
        "an upper bound joins it there rather than outside: {bounded}"
    );
}

/// Expected behaviour: a deployment still open covers the window, and the parameter filter is in
/// the statement only when one was asked for. Reading `deployed_until` as a closed bound would
/// drop the band an instrument is on right now.
#[test]
fn an_open_deployment_is_in_the_window_and_the_parameter_filter_is_optional() {
    use sea_orm::sea_query::PostgresQueryBuilder;

    let site = uuid::Uuid::from_u128(1);
    let parameter = uuid::Uuid::from_u128(2);
    let start = chrono::DateTime::from_timestamp(0, 0).expect("epoch is a timestamp");
    let end = chrono::DateTime::from_timestamp(86_400, 0).expect("a day later is a timestamp");

    let unfiltered =
        super::sensor_identity_bands_query(site, start, end, None).to_string(PostgresQueryBuilder);
    assert!(
        unfiltered.contains(r#""d"."deployed_until" IS NULL OR "d"."deployed_until" >"#),
        "an open deployment covers everything after its start: {unfiltered}"
    );
    assert!(
        unfiltered.contains(r#"INNER JOIN "sensors""#),
        "the band names the instrument that held the slot: {unfiltered}"
    );
    assert!(
        !unfiltered.contains(r#""d"."parameter_id" IN"#),
        "no parameter filter was asked for: {unfiltered}"
    );
    assert!(
        unfiltered.ends_with(r#"ORDER BY "d"."parameter_id" ASC, "d"."deployed_from" ASC"#),
        "bands arrive grouped by parameter, oldest first: {unfiltered}"
    );

    let filtered = super::sensor_identity_bands_query(site, start, end, Some(&[parameter]))
        .to_string(PostgresQueryBuilder);
    assert!(
        filtered.contains(r#""d"."parameter_id" IN"#),
        "the asked-for parameters confine the bands: {filtered}"
    );
    let empty = super::sensor_identity_bands_query(site, start, end, Some(&[]))
        .to_string(PostgresQueryBuilder);
    assert_eq!(
        empty, unfiltered,
        "an empty filter list confines nothing: {empty}"
    );
}

/// Expected behaviour: markers are the curves of the instruments deployed here over the window,
/// and a curve with no parameter has no series to sit on. Dropping the subquery would plot every
/// curve in the database on the site's charts.
#[test]
fn a_marker_belongs_to_an_instrument_deployed_here_and_names_its_parameter() {
    use sea_orm::sea_query::PostgresQueryBuilder;

    let site = uuid::Uuid::from_u128(1);
    let start = chrono::DateTime::from_timestamp(0, 0).expect("epoch is a timestamp");
    let end = chrono::DateTime::from_timestamp(86_400, 0).expect("a day later is a timestamp");

    let markers = super::sensor_calibration_markers_query(site, start, end, None)
        .to_string(PostgresQueryBuilder);
    assert!(
        markers.contains(r#"IN (SELECT DISTINCT "d"."sensor_id""#),
        "only instruments deployed at this site carry markers: {markers}"
    );
    assert!(
        markers.contains(r#""c"."parameter_id" IS NOT NULL"#),
        "a curve with no parameter has no series to sit on: {markers}"
    );
    assert!(
        markers.contains(r#""c"."valid_until" IS NULL OR "c"."valid_until" >"#),
        "a curve still in force overlaps the window: {markers}"
    );
}

#[test]
fn test_every_rollup_is_reachable_by_a_resolution_keyword() {
    use crate::common::aggregates::Resolution;
    use crate::routes::private::sites::service::resolution_of;

    let keywords = [
        "hourly", "6hourly", "12hourly", "daily", "weekly", "monthly",
    ];
    let reached: Vec<Resolution> = keywords.iter().filter_map(|k| resolution_of(k)).collect();
    assert_eq!(reached, Resolution::ALL.to_vec());
    assert_eq!(resolution_of("6h"), None);
}

#[test]
fn test_the_bucket_width_matches_the_view_the_resolution_names() {
    use crate::common::aggregates::Resolution;
    use crate::routes::private::sites::service::bucket_interval;

    assert_eq!(bucket_interval(Resolution::SixHourly), "6 hours");
    assert_eq!(bucket_interval(Resolution::TwelveHourly), "12 hours");
}

/// Expected behaviour: `frequency` is the slot's declared cadence, whatever its rows hold. A slot
/// declared `low` that still carries a week of logger readings reads as `low`, because the
/// declaration is what the chain and the stream engine divide on.
#[test]
fn frequency_is_the_declared_cadence_and_not_what_the_rows_hold() {
    use super::{ParameterExtent, build_parameter_response};
    use std::collections::HashMap;

    let parameter_id = uuid::Uuid::from_u128(2);
    let slot = slot_with_cadence(parameter_id, "low");
    let mut extents = HashMap::new();
    extents.insert(
        parameter_id,
        ParameterExtent {
            data_start: None,
            data_end: None,
            reading_count: 12,
            spot_count: 4,
            continuous_count: 8,
        },
    );

    let response = build_parameter_response(slot, &HashMap::new(), &extents, &HashMap::new());

    assert_eq!(response.frequency, "low");
    // The extent stays what it is: both arms of history are legitimately there.
    assert!(response.has_spot);
    assert!(response.has_continuous);
}

/// Expected behaviour: a slot with no readings at all reports its declaration just the same, so a
/// new one opens on the right chart mode with nothing to observe.
#[test]
fn an_empty_slot_reports_its_declaration() {
    use super::{ParameterExtent, build_parameter_response};
    use std::collections::HashMap;

    let parameter_id = uuid::Uuid::from_u128(3);
    let response = build_parameter_response(
        slot_with_cadence(parameter_id, "high"),
        &HashMap::new(),
        &HashMap::<uuid::Uuid, ParameterExtent>::new(),
        &HashMap::new(),
    );

    assert_eq!(response.frequency, "high");
    assert!(!response.has_spot);
    assert!(!response.has_continuous);
}

fn slot_with_cadence(
    parameter_id: uuid::Uuid,
    cadence: &str,
) -> crate::routes::private::site_parameters::Model {
    crate::routes::private::site_parameters::Model {
        id: uuid::Uuid::from_u128(10),
        site_id: uuid::Uuid::from_u128(1),
        parameter_id,
        name: "Turbidity".to_string(),
        sensor_type: String::new(),
        decimal_places: None,
        sample_interval_sec: None,
        is_active: Some(true),
        is_public: Some(false),
        needs_review: false,
        entry_mode: "manual".to_string(),
        cadence: cadence.to_string(),
        instrument_sensor_id: None,
        variable_mappings: None,
        created_at: None,
        updated_at: None,
        discovered_at: None,
        parameter: Vec::new(),
    }
}

fn exported_parameter(
    code: &str,
    values: Vec<Option<f64>>,
    withdrawn: Option<Vec<Option<bool>>>,
    unverified: Vec<Option<bool>>,
) -> super::ParameterData {
    super::ParameterData {
        id: uuid::Uuid::from_u128(10),
        parameter_id: uuid::Uuid::from_u128(11),
        code: code.to_string(),
        name: code.to_string(),
        display_name: None,
        sensor_type: code.to_string(),
        units: None,
        decimal_places: None,
        values,
        severities: None,
        flagged: None,
        flag_reasons: None,
        measurement_types: None,
        calibration_ids: None,
        standard_curve_ids: None,
        samples: None,
        origins: None,
        withdrawn,
        unverified: Some(unverified),
        withdrawn_count: None,
    }
}

fn export_times(count: i64) -> Vec<chrono::DateTime<chrono::Utc>> {
    let start = chrono::DateTime::<chrono::Utc>::from_timestamp(1_780_000_000, 0).unwrap();
    (0..count)
        .map(|i| start + chrono::Duration::minutes(i))
        .collect()
}

/// Expected behaviour: the file marks a retracted instant and a pending entry exactly as the JSON
/// body does, so neither is exported as an ordinary value.
#[test]
fn test_readings_table_marks_withdrawn_and_unverified_points() {
    let times = export_times(2);
    let params = [
        exported_parameter(
            "DOC",
            vec![Some(1.5), Some(2.5)],
            Some(vec![Some(true), Some(false)]),
            vec![Some(false), Some(true)],
        ),
        exported_parameter(
            "TN",
            vec![None, Some(3.0)],
            Some(vec![None, Some(false)]),
            vec![None, Some(false)],
        ),
    ];
    let table = super::readings_table(&times, &params, false, None);

    let header = table.header_line();
    let columns: Vec<&str> = header.split(',').collect();
    for name in [
        "DOC_withdrawn",
        "TN_withdrawn",
        "DOC_unverified",
        "TN_unverified",
    ] {
        assert!(columns.contains(&name), "{name} missing from {header}");
    }
    let cell = |row: usize, name: &str| {
        let at = columns.iter().position(|c| *c == name).unwrap();
        table.csv_line(row).split(',').nth(at).unwrap().to_string()
    };
    assert_eq!(cell(0, "DOC"), "1.5");
    assert_eq!(cell(0, "DOC_withdrawn"), "true");
    assert_eq!(cell(1, "DOC_withdrawn"), "false");
    assert_eq!(cell(0, "DOC_unverified"), "false");
    assert_eq!(cell(1, "DOC_unverified"), "true");
    assert_eq!(
        cell(0, "TN_unverified"),
        "",
        "no TN point at the first instant"
    );

    let first: serde_json::Value = serde_json::from_str(&table.ndjson_line(0)).unwrap();
    assert_eq!(first["DOC_withdrawn"], serde_json::json!(true));
    assert_eq!(first["DOC_unverified"], serde_json::json!(false));
    let second: serde_json::Value = serde_json::from_str(&table.ndjson_line(1)).unwrap();
    assert_eq!(second["DOC_unverified"], serde_json::json!(true));
}

/// Expected behaviour: the pending mark is on every export, and the retraction column appears
/// only when the request served retracted instants.
#[test]
fn test_readings_table_carries_unverified_without_any_opt_in() {
    let times = export_times(1);
    let params = [exported_parameter(
        "DOC",
        vec![Some(1.5)],
        None,
        vec![Some(true)],
    )];
    let header = super::readings_table(&times, &params, false, None).header_line();
    assert_eq!(header, "time,DOC,DOC_unverified");
}

/// Scenario: a daily read whose window ends at noon serves that day's bucket whole.
/// Expected behaviour: the flag count is bounded by the same buckets the rollup read serves, so a
/// flagged reading in the afternoon of the last day is counted on the bucket it changed.
#[test]
fn test_flagged_buckets_query_bounds_by_the_served_buckets() {
    use sea_orm::sea_query::PostgresQueryBuilder;

    use crate::common::aggregates::Resolution;

    let rollup = super::aggregate_buckets_sql(Resolution::Daily, false);
    assert!(
        rollup.contains("bucket >= $3") && rollup.contains("bucket <= $4"),
        "the rollup serves every bucket starting inside the window: {rollup}"
    );

    let start = chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
        .expect("start parses")
        .to_utc();
    let end = chrono::DateTime::parse_from_rfc3339("2026-09-10T12:00:00Z")
        .expect("end parses")
        .to_utc();
    let flagged = super::flagged_buckets_query(
        Resolution::Daily,
        uuid::Uuid::from_u128(1),
        &[uuid::Uuid::from_u128(2)],
        false,
        start,
        end,
    )
    .to_string(PostgresQueryBuilder);

    let bucket = r#"time_bucket(CAST('1 day' AS interval), "time")"#;
    assert!(
        flagged.contains(&format!("{bucket} >= '2026-09-01 00:00:00.000000 +00:00'"))
            && flagged.contains(&format!("{bucket} <= '2026-09-10 12:00:00.000000 +00:00'")),
        "the flag count reads the rollup's buckets: {flagged}"
    );
    assert!(
        !flagged.contains(r#""time" <= "#),
        "a raw upper bound would cut the last bucket short: {flagged}"
    );
    assert!(
        flagged.contains(
            r#""time" < CAST('2026-09-10 12:00:00.000000 +00:00' AS timestamptz) + CAST('1 day' AS interval)"#
        ),
        "chunk exclusion still sees an upper bound one width past the window: {flagged}"
    );
}

// --- The site readings request ---

fn readings_query() -> super::SiteReadingsQuery {
    super::SiteReadingsQuery {
        start: None,
        end: None,
        sensor_types: None,
        parameter_ids: None,
        format: "json".to_string(),
        alarms: None,
        measurement_type: None,
        include_flagged: None,
        include_flags: None,
        include_replicates: None,
        sample_id: None,
        include_measurement_type: None,
        include_curves: None,
        include_sample_stats: None,
        include_origin: None,
        include_withdrawn: None,
    }
}

fn at(seconds: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(seconds, 0).expect("a timestamp")
}

fn reading(
    parameter: u128,
    seconds: i64,
    replicate_index: Option<i16>,
    value: f64,
) -> super::ReadingRow {
    super::ReadingRow {
        parameter_id: uuid::Uuid::from_u128(parameter),
        time: at(seconds).fixed_offset(),
        replicate_index,
        stream_id: uuid::Uuid::from_u128(99),
        value,
        severity: None,
        is_flagged: Some(false),
        flag_reason: None,
        measurement_type: Some("continuous".to_string()),
        sample_id: None,
        calibration_id: None,
        standard_curve_id: None,
        withdrawn: Some(false),
        unverified: Some(false),
    }
}

fn request(query: &super::SiteReadingsQuery) -> super::ReadingsRequest {
    super::ReadingsRequest::from_query(query, 7, at(30 * 86_400)).expect("the query is valid")
}

#[test]
fn test_readings_request_opens_the_window_lookback_days_before_now() {
    let open = request(&readings_query());
    // 30 days after the epoch, less the seven-day lookback
    assert_eq!(open.start, at(23 * 86_400));
    assert_eq!(open.end, None);

    let named = request(&super::SiteReadingsQuery {
        start: Some(at(0)),
        end: Some(at(86_400)),
        ..readings_query()
    });
    assert_eq!((named.start, named.end), (at(0), Some(at(86_400))));
}

#[test]
fn test_readings_request_refuses_a_window_ending_before_it_starts() {
    let query = super::SiteReadingsQuery {
        start: Some(at(86_400)),
        end: Some(at(0)),
        ..readings_query()
    };
    assert!(super::ReadingsRequest::from_query(&query, 7, at(0)).is_err());
}

/// Expected behaviour: a row per replicate and a row per instant are two files, so asking for
/// both is refused rather than served as one of them.
#[test]
fn test_readings_request_refuses_replicates_with_sample_stats() {
    let both = super::SiteReadingsQuery {
        include_replicates: Some(true),
        include_sample_stats: Some(true),
        ..readings_query()
    };
    let refused = super::ReadingsRequest::from_query(&both, 7, at(0));
    assert!(
        matches!(refused, Err(crate::error::AppError::BadRequest(ref m)) if m.contains("cannot be combined")),
        "{refused:?}"
    );

    let by_sample = super::SiteReadingsQuery {
        sample_id: Some(uuid::Uuid::from_u128(5)),
        include_sample_stats: Some(true),
        ..readings_query()
    };
    assert!(super::ReadingsRequest::from_query(&by_sample, 7, at(0)).is_err());
}

#[test]
fn test_readings_request_sample_id_asks_for_replicates() {
    let by_sample = request(&super::SiteReadingsQuery {
        sample_id: Some(uuid::Uuid::from_u128(5)),
        ..readings_query()
    });
    assert!(by_sample.replicates);
    assert!(!request(&readings_query()).replicates);
}

#[test]
fn test_readings_request_refuses_an_unknown_measurement_type() {
    let query = super::SiteReadingsQuery {
        measurement_type: Some("hourly".to_string()),
        ..readings_query()
    };
    assert!(super::ReadingsRequest::from_query(&query, 7, at(0)).is_err());

    let empty = request(&super::SiteReadingsQuery {
        measurement_type: Some(String::new()),
        ..readings_query()
    });
    assert_eq!(empty.measurement_type, "");
}

/// Expected behaviour: the flag arrays are on by default and every other annotation is opt-in.
#[test]
fn test_readings_request_annotations_default_to_flags_only() {
    let plain = request(&readings_query());
    let a = plain.annotations;
    assert!(a.flagged);
    assert!(
        !(a.alarms || a.measurement_type || a.sample_stats || a.curves || a.origin || a.withdrawn)
    );
    assert!(!plain.flag_columns);
}

/// Expected behaviour: the withdrawn count is served on the collapsed view unless the filter
/// names the continuous arm alone.
#[test]
fn test_readings_request_counts_withdrawn_off_the_replicate_and_continuous_views() {
    assert!(request(&readings_query()).counts_withdrawn());
    let spot = super::SiteReadingsQuery {
        measurement_type: Some("spot".to_string()),
        ..readings_query()
    };
    assert!(request(&spot).counts_withdrawn());
    let continuous = super::SiteReadingsQuery {
        measurement_type: Some("continuous".to_string()),
        ..readings_query()
    };
    assert!(!request(&continuous).counts_withdrawn());
    let replicates = super::SiteReadingsQuery {
        include_replicates: Some(true),
        ..readings_query()
    };
    assert!(!request(&replicates).counts_withdrawn());
}

#[test]
fn test_series_arms_by_measurement_type() {
    let arms = |filter: &str| {
        let a = super::series_arms(filter);
        (a.continuous, a.spot, a.continuous_type)
    };
    assert_eq!(arms(""), (true, true, None));
    assert_eq!(arms("continuous"), (true, false, None));
    assert_eq!(arms("spot"), (false, true, None));
    assert_eq!(arms("derived"), (true, false, Some("derived".to_string())));
}

#[test]
fn test_parameter_id_filter_refuses_a_list_naming_no_uuid() {
    assert_eq!(
        super::parameter_id_filter(None).expect("absent is no filter"),
        None
    );
    assert!(super::parameter_id_filter(Some("not-a-uuid, ,")).is_err());

    let one = uuid::Uuid::from_u128(7);
    assert_eq!(
        super::parameter_id_filter(Some(&format!(" {one} ,junk"))).expect("one parses"),
        Some(vec![one])
    );
}

#[test]
fn test_sensor_type_filter_trims_each_type() {
    assert_eq!(super::sensor_type_filter(None), None);
    assert_eq!(
        super::sensor_type_filter(Some("Depth, CDOM")),
        Some(vec!["Depth".to_string(), "CDOM".to_string()])
    );
}

/// Expected behaviour: the replicate export leaves out a row the source has taken back unless
/// the request asks for it, as every other serving path does.
#[test]
fn test_replicate_rows_query_excludes_withdrawn_unless_asked() {
    use sea_orm::sea_query::PostgresQueryBuilder;

    let site = uuid::Uuid::from_u128(1);
    let parameters = [uuid::Uuid::from_u128(2)];
    let replicates = super::SiteReadingsQuery {
        include_replicates: Some(true),
        ..readings_query()
    };
    let default = super::replicate_rows_query(site, &parameters, &request(&replicates))
        .to_string(PostgresQueryBuilder);
    assert!(
        default.contains(r#""r"."withdrawn_at" IS NULL"#),
        "a retracted row is left out: {default}"
    );
    assert!(
        default.contains(
            r#"ORDER BY "r"."parameter_id" ASC, "r"."time" ASC, "r"."replicate_index" ASC"#
        ),
        "rows come in axis order: {default}"
    );

    let withdrawn = super::SiteReadingsQuery {
        include_withdrawn: Some(true),
        ..replicates
    };
    let asked = super::replicate_rows_query(site, &parameters, &request(&withdrawn))
        .to_string(PostgresQueryBuilder);
    assert!(!asked.contains(r#""r"."withdrawn_at" IS NULL"#), "{asked}");
}

/// Expected behaviour: the collapsed series unions only the arms its filter reaches, joins the
/// thresholds only under `alarms`, and drops flagged rows only when asked to.
#[test]
fn test_served_series_query_reaches_the_arms_its_filter_names() {
    use sea_orm::sea_query::PostgresQueryBuilder;

    let site = uuid::Uuid::from_u128(1);
    let parameters = [uuid::Uuid::from_u128(2)];
    let rendered = |query: super::SiteReadingsQuery| {
        super::served_series_query(site, &parameters, &request(&query))
            .to_string(PostgresQueryBuilder)
    };

    let both = rendered(readings_query());
    assert!(both.contains("UNION ALL"), "{both}");
    assert!(
        both.contains("DISTINCT ON"),
        "the spot arm is one row per instant: {both}"
    );
    assert!(
        !both.contains("is_flagged IS NOT TRUE"),
        "flagged rows are served: {both}"
    );
    assert!(
        !both.contains("warning_min"),
        "no thresholds without alarms: {both}"
    );

    let spot = rendered(super::SiteReadingsQuery {
        measurement_type: Some("spot".to_string()),
        include_flagged: Some(false),
        alarms: Some(true),
        ..readings_query()
    });
    assert!(!spot.contains("UNION ALL"), "{spot}");
    assert!(spot.contains("is_flagged IS NOT TRUE"), "{spot}");
    assert!(spot.contains("warning_min"), "{spot}");

    let derived = rendered(super::SiteReadingsQuery {
        measurement_type: Some("derived".to_string()),
        ..readings_query()
    });
    assert!(!derived.contains("DISTINCT ON"), "{derived}");
    assert!(
        derived.contains(r#""r"."measurement_type" = 'derived'"#),
        "the continuous arm is narrowed to the named type: {derived}"
    );
}

/// Expected behaviour: the replicate view's axis is every `(time, replicate_index)` pair any
/// parameter has, sorted; the collapsed view's is every instant. Filling by position instead
/// would date one parameter's values to another's instants.
#[test]
fn test_row_axis_is_the_sorted_union_of_every_parameters_keys() {
    let rows = [
        reading(1, 600, Some(1), 1.0),
        reading(1, 600, Some(0), 2.0),
        reading(2, 0, Some(0), 3.0),
        reading(2, 600, Some(0), 4.0),
    ];

    let replicates = super::RowAxis::over(&rows, true);
    assert_eq!(replicates.times(), vec![at(0), at(600), at(600)]);
    assert_eq!(replicates.replicate_indices(), Some(vec![0, 0, 1]));
    assert_eq!(replicates.position(&rows[0]), Some(2));

    let collapsed = [
        reading(1, 600, None, 1.0),
        reading(2, 0, None, 3.0),
        reading(2, 600, None, 4.0),
    ];
    let instants = super::RowAxis::over(&collapsed, false);
    assert_eq!(instants.times(), vec![at(0), at(600)]);
    assert_eq!(instants.replicate_indices(), None);
    assert_eq!(instants.position(&collapsed[0]), Some(1));
}

/// Expected behaviour: each slot's values land at their own instants on the shared axis, and an
/// annotation is present only when it was asked for.
#[test]
fn test_slot_series_places_each_value_at_its_instant() {
    let first = slot_with_cadence(uuid::Uuid::from_u128(1), "high");
    let second = crate::routes::private::site_parameters::Model {
        id: uuid::Uuid::from_u128(20),
        ..slot_with_cadence(uuid::Uuid::from_u128(2), "high")
    };
    let rows = vec![reading(1, 600, None, 1.5), reading(2, 0, None, 3.0)];
    let axis = super::RowAxis::over(&rows, false);
    let plain = request(&readings_query());
    let origin = super::OriginRef {
        stream_id: uuid::Uuid::from_u128(99),
        source_system: "vaisala".to_string(),
        source_key: "1270".to_string(),
    };
    let context = super::SeriesContext {
        sample_stats: std::collections::HashMap::new(),
        origins: Some(std::collections::HashMap::from([(first.id, vec![origin])])),
        withdrawn_counts: Some(std::collections::HashMap::from([(
            uuid::Uuid::from_u128(1),
            2,
        )])),
    };

    let series = super::slot_series(
        &[first, second],
        &std::collections::HashMap::new(),
        rows,
        &axis,
        plain.annotations,
        &context,
    );

    assert_eq!(series[0].values, vec![None, Some(1.5)]);
    assert_eq!(series[1].values, vec![Some(3.0), None]);
    assert_eq!(series[0].flagged, Some(vec![None, Some(false)]));
    assert_eq!(series[0].unverified, Some(vec![None, Some(false)]));
    assert!(series[0].severities.is_none() && series[0].samples.is_none());
    assert_eq!(series[0].origins.as_ref().map(Vec::len), Some(1));
    assert_eq!(series[1].origins.as_ref().map(Vec::len), Some(0));
    assert_eq!(series[0].withdrawn_count, Some(2));
    assert_eq!(series[1].withdrawn_count, Some(0));
}
