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
    use crate::routes::private::sites::service::{bucket_interval, resolution_of};

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
