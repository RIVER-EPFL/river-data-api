use chrono::{DateTime, TimeZone, Utc};
use uuid::Uuid;

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

/// Core's struct as it goes on the wire, then read back as the API reads it. A field core gained
/// and the API did not fails here rather than in a field assertion, on every receiver that carries
/// `deny_unknown_fields`; `the_receivers_that_refuse_an_unknown_field` says which those are.
fn through<S: serde::Serialize, D: serde::de::DeserializeOwned>(sent: &S) -> D {
    let json = serde_json::to_value(sent).expect("the sender serialises");
    serde_json::from_value(json.clone())
        .unwrap_or_else(|e| panic!("the receiver refuses what the sender sends: {e}\n{json:#}"))
}

#[test]
fn an_ingest_reading_arrives_whole() {
    use river_data_core::models::IngestReading;
    let sent = IngestReading {
        time: at("2026-01-15T10:00:00Z"),
        raw_value: 412.5,
        replicate_index: 2,
        sensor_id: Some(id(1)),
        calibration_id: Some(id(2)),
        deployment_id: Some(id(3)),
        measurement_type: Some("spot".to_string()),
        standard_curve_id: Some(id(4)),
    };
    let got: river_db::routes::private::readings::ingest::IngestReading = through(&sent);

    assert_eq!(got.time, sent.time);
    assert!((got.raw_value - sent.raw_value).abs() < f64::EPSILON);
    assert_eq!(got.replicate_index, sent.replicate_index);
    assert_eq!(got.sensor_id, sent.sensor_id);
    assert_eq!(got.calibration_id, sent.calibration_id);
    assert_eq!(got.deployment_id, sent.deployment_id);
    assert_eq!(got.measurement_type, sent.measurement_type);
    assert_eq!(got.standard_curve_id, sent.standard_curve_id);
}

#[test]
fn a_stream_registration_arrives_whole() {
    use river_data_core::models::{RegisterStreamRequest, ReplicateSpec};
    let sent = RegisterStreamRequest {
        source_system: "cnet".to_string(),
        source_key: "FP1:DOC_avg_ppb:reps".to_string(),
        source_name: Some("DOC".to_string()),
        source_path: Some("cnet/FP1".to_string()),
        metadata: serde_json::json!({ "station": "FP1" }),
        measurement_type: Some("spot".to_string()),
        sensor_id: Some(id(5)),
        replicates: Some(ReplicateSpec {
            source_columns: vec!["DOC_A".to_string(), "DOC_B".to_string()],
            portal_mean_column: Some("DOC_avg_ppb".to_string()),
            portal_sd_column: Some("DOC_sd_ppb".to_string()),
            curve_ref_column: Some("doc_std_curve_id".to_string()),
            calc: Some("calcMean".to_string()),
            sd_estimator: Some("population".to_string()),
        }),
        decimal_places: Some(2),
    };
    let got: river_db::routes::private::data_streams::views::RegisterStreamRequest = through(&sent);

    assert_eq!(got.source_system, sent.source_system);
    assert_eq!(got.source_key, sent.source_key);
    assert_eq!(got.source_name, sent.source_name);
    assert_eq!(got.source_path, sent.source_path);
    assert_eq!(got.metadata, sent.metadata);
    assert_eq!(got.measurement_type, sent.measurement_type);
    assert_eq!(got.sensor_id, sent.sensor_id);
    assert_eq!(got.decimal_places, sent.decimal_places);

    let declared = sent.replicates.expect("the spec was sent");
    let received = got.replicates.expect("the spec arrives");
    assert_eq!(received.source_columns, declared.source_columns);
    assert_eq!(received.portal_mean_column, declared.portal_mean_column);
    assert_eq!(received.portal_sd_column, declared.portal_sd_column);
    assert_eq!(received.curve_ref_column, declared.curve_ref_column);
    assert_eq!(received.calc, declared.calc);
    assert_eq!(received.sd_estimator, declared.sd_estimator);
    assert!(
        received.assignments.is_empty(),
        "the caller never authors the index mapping; the register path pins it"
    );
}

/// The sharpest of the nine: the window round-trips. The API echoes it as `accepted_window` and
/// the client treats a missing echo as a hard error, which is what stops a stale API image
/// downgrading a reconciled source to append-only.
#[test]
fn a_source_window_round_trips_in_both_directions() {
    use river_data_core::models::SourceWindow;
    let sent = SourceWindow {
        from: at("2026-01-01T00:00:00Z"),
        to: at("2026-02-01T00:00:00Z"),
        source_rows_read: 512,
        dropped_times: vec![at("2026-01-15T10:00:00Z")],
        content_digest: Some("fnv1a:deadbeef".to_string()),
    };
    let server: river_db::routes::private::readings::reconcile::SourceWindow = through(&sent);
    assert_eq!(server.from, sent.from);
    assert_eq!(server.to, sent.to);
    assert_eq!(server.source_rows_read, sent.source_rows_read);
    assert_eq!(server.dropped_times, sent.dropped_times);
    assert_eq!(server.content_digest, sent.content_digest);

    let echoed: SourceWindow = through(&server);
    assert_eq!(echoed.from, sent.from);
    assert_eq!(echoed.to, sent.to);
    assert_eq!(echoed.source_rows_read, sent.source_rows_read);
    assert_eq!(echoed.dropped_times, sent.dropped_times);
    assert_eq!(echoed.content_digest, sent.content_digest);
}

#[test]
fn a_column_assignment_round_trips_in_both_directions() {
    use river_data_core::models::ColumnAssignment;
    let sent = ColumnAssignment {
        column: "DOC_C".to_string(),
        index: 2,
        retired: true,
    };
    let server: river_db::routes::private::data_streams::replicates::ColumnAssignment =
        through(&sent);
    assert_eq!(server.column, sent.column);
    assert_eq!(server.index, sent.index);
    assert_eq!(server.retired, sent.retired);

    // The client assigns each value's replicate index from the mapping the register response
    // returns, so this direction is the one that decides where a reading is stored.
    let returned: ColumnAssignment = through(&server);
    assert_eq!(returned.column, sent.column);
    assert_eq!(returned.index, sent.index);
    assert_eq!(returned.retired, sent.retired);
}

#[test]
fn a_group_audit_arrives_whole() {
    use river_data_core::models::GroupAudit;
    let sent = GroupAudit {
        time: at("2026-01-15T10:00:00Z"),
        expected_mean: Some(412.5),
        expected_sd: Some(0.7),
        expected_n: Some(3),
    };
    let got: river_db::routes::private::sync::replicate_audit::GroupAudit = through(&sent);
    assert_eq!(got.time, sent.time);
    assert_eq!(got.expected_mean, sent.expected_mean);
    assert_eq!(got.expected_sd, sent.expected_sd);
    assert_eq!(got.expected_n, sent.expected_n);
}

#[test]
fn an_instrument_registration_arrives_whole() {
    use river_data_core::models::SensorUpsert;
    let sent = SensorUpsert {
        source_key: "sensor_inventory:62".to_string(),
        name: "DOC corr".to_string(),
        serial_number: Some("SN-62".to_string()),
        manufacturer: Some("Shimadzu".to_string()),
        model: Some("TOC-L".to_string()),
        notes: Some("lab bench".to_string()),
        is_lab_instrument: true,
        metadata: Some(serde_json::json!({ "bench": 2 })),
    };
    // The API requires `source_system`, which the client injects on send rather than carrying on
    // the struct; everything else is the struct as it stands.
    let mut json = serde_json::to_value(&sent).expect("the upsert serialises");
    json["source_system"] = serde_json::json!("cnet");
    let got: river_db::routes::private::sensors::register::RegisterSensorRequest =
        serde_json::from_value(json).expect("the API reads what core sends");

    assert_eq!(got.source_key, sent.source_key);
    assert_eq!(got.name, sent.name);
    assert_eq!(got.serial_number, sent.serial_number);
    assert_eq!(got.manufacturer, sent.manufacturer);
    assert_eq!(got.model, sent.model);
    assert_eq!(got.notes, sent.notes);
    assert_eq!(got.is_lab_instrument, sent.is_lab_instrument);
    assert_eq!(got.metadata, sent.metadata);
    assert_eq!(
        got.data_frequency, "high",
        "core carries no cadence, so every synced instrument takes the API's default; \
         a lab instrument the API would mint as 'low' arrives 'high' (C84)"
    );
}

#[test]
fn a_standard_curve_registration_arrives_whole() {
    use river_data_core::models::StandardCurveUpsert;
    let sent = StandardCurveUpsert {
        source_key: "standard_curves:17".to_string(),
        instrument_label: "DOC corr".to_string(),
        slope: 1.5,
        intercept: -0.25,
        r_squared: Some(0.998),
        name: Some("2026-01 DOC".to_string()),
        fitted_on: Some(
            Utc.with_ymd_and_hms(2026, 1, 15, 0, 0, 0)
                .unwrap()
                .date_naive(),
        ),
    };
    let mut json = serde_json::to_value(&sent).expect("the upsert serialises");
    json["source_system"] = serde_json::json!("cnet");
    let got: river_db::routes::private::sensors::standard_curves::views::RegisterStandardCurveRequest =
        serde_json::from_value(json).expect("the API reads what core sends");

    assert_eq!(got.source_key, sent.source_key);
    assert_eq!(got.instrument_label, sent.instrument_label);
    assert!((got.slope - sent.slope).abs() < f64::EPSILON);
    assert!((got.intercept - sent.intercept).abs() < f64::EPSILON);
    assert_eq!(got.r_squared, sent.r_squared);
    assert_eq!(got.name, sent.name);
    assert_eq!(got.fitted_on, sent.fitted_on);
    assert_eq!(
        got.notes, None,
        "the API has a notes column core cannot send, so a portal curve's note never arrives (C84)"
    );
}

#[test]
fn an_annotation_registration_arrives_whole_and_its_outcome_reads_back() {
    use river_data_core::models::{AnnotationMapping, AnnotationUpsert};
    let sent = AnnotationUpsert {
        source_key: "annotations:9".to_string(),
        stream_id: id(6),
        time: at("2026-01-15T10:00:00Z"),
        category: "audit".to_string(),
        text: "corrected against curve 17".to_string(),
        standard_curve_id: Some(id(7)),
    };
    let got: river_db::routes::private::annotations::register::AnnotationItem = through(&sent);
    assert_eq!(got.source_key, sent.source_key);
    assert_eq!(got.stream_id, sent.stream_id);
    assert_eq!(got.time, sent.time);
    assert_eq!(got.category, sent.category);
    assert_eq!(got.text, sent.text);
    assert_eq!(got.standard_curve_id, sent.standard_curve_id);

    let outcome = river_db::routes::private::annotations::register::AnnotationOutcome {
        source_key: sent.source_key.clone(),
        id: Some(id(8)),
        status: "created".to_string(),
    };
    let read: AnnotationMapping = through(&outcome);
    assert_eq!(read.source_key, outcome.source_key);
    assert_eq!(read.id, outcome.id);
    assert_eq!(read.status, outcome.status);
}

#[test]
fn a_note_registration_arrives_whole_and_its_outcome_reads_back() {
    use river_data_core::models::{NoteMapping, NoteUpsert};
    let sent = NoteUpsert {
        source_key: "notes:1".to_string(),
        site_name: "FP1".to_string(),
        text: "bank collapsed upstream".to_string(),
        verified: true,
    };
    let got: river_db::routes::private::notes::register::NoteItem = through(&sent);
    assert_eq!(got.source_key, sent.source_key);
    assert_eq!(got.site_name, sent.site_name);
    assert_eq!(got.text, sent.text);
    assert_eq!(got.verified, sent.verified);

    let outcome = river_db::routes::private::notes::register::NoteOutcome {
        source_key: sent.source_key.clone(),
        id: None,
        status: "unresolved".to_string(),
    };
    let read: NoteMapping = through(&outcome);
    assert_eq!(read.source_key, outcome.source_key);
    assert_eq!(read.id, outcome.id);
    assert_eq!(read.status, outcome.status);
}

/// The status-event pair is the third recorded drift: the API can attribute an event to an
/// instrument and core's two-field version can never populate it.
#[test]
fn a_status_event_arrives_whole_and_names_no_instrument() {
    use river_data_core::models::IngestStatusEvent;
    let sent = IngestStatusEvent {
        time: at("2026-01-15T10:00:00Z"),
        value: "unreachable".to_string(),
    };
    let got: river_db::routes::private::readings::ingest::IngestStatusEvent = through(&sent);
    assert_eq!(got.time, sent.time);
    assert_eq!(got.value, sent.value);
    assert_eq!(
        got.sensor_id, None,
        "core carries no sensor_id, so a synced status event is never attributed (C84)"
    );
}

/// Which side of the contract a new field on the sender lands on. A receiver with
/// `deny_unknown_fields` refuses the whole request, which is loud; one without it drops the field
/// and stores a row missing what the source sent, which is the shape the three recorded drifts
/// took. This asserts the split as it stands so neither half moves unnoticed.
#[test]
fn the_receivers_that_refuse_an_unknown_field() {
    fn refuses<D: serde::de::DeserializeOwned>(mut json: serde_json::Value) -> bool {
        json["a_field_the_sender_gained"] = serde_json::json!(1);
        serde_json::from_value::<D>(json).is_err()
    }

    let reading = serde_json::json!({ "time": "2026-01-15T10:00:00Z", "raw_value": 1.0 });
    assert!(refuses::<
        river_db::routes::private::readings::ingest::IngestReading,
    >(reading));

    let window = serde_json::json!({
        "from": "2026-01-01T00:00:00Z", "to": "2026-02-01T00:00:00Z", "source_rows_read": 1,
    });
    assert!(refuses::<
        river_db::routes::private::readings::reconcile::SourceWindow,
    >(window));

    let audit = serde_json::json!({ "time": "2026-01-15T10:00:00Z" });
    assert!(refuses::<
        river_db::routes::private::sync::replicate_audit::GroupAudit,
    >(audit));

    let instrument = serde_json::json!({
        "source_system": "cnet", "source_key": "sensor_inventory:62", "name": "DOC corr",
    });
    assert!(refuses::<
        river_db::routes::private::sensors::register::RegisterSensorRequest,
    >(instrument));

    let annotation = serde_json::json!({
        "source_key": "annotations:9", "stream_id": "00000000-0000-0000-0000-000000000006",
        "time": "2026-01-15T10:00:00Z", "category": "audit", "text": "x",
    });
    assert!(refuses::<
        river_db::routes::private::annotations::register::AnnotationItem,
    >(annotation));

    let note = serde_json::json!({ "source_key": "notes:1", "site_name": "FP1", "text": "x" });
    assert!(refuses::<
        river_db::routes::private::notes::register::NoteItem,
    >(note));

    // The three that do not refuse. A field core gains here is dropped in silence, which is why
    // the value assertions above are the guard for them rather than the deserialize.
    let stream = serde_json::json!({
        "source_system": "cnet", "source_key": "FP1:DOC", "source_name": null, "source_path": null,
    });
    assert!(!refuses::<
        river_db::routes::private::data_streams::views::RegisterStreamRequest,
    >(stream));

    let curve = serde_json::json!({
        "source_system": "cnet", "source_key": "standard_curves:17",
        "instrument_label": "DOC corr", "slope": 1.0, "intercept": 0.0,
    });
    assert!(!refuses::<
        river_db::routes::private::sensors::standard_curves::views::RegisterStandardCurveRequest,
    >(curve));

    let assignment = serde_json::json!({ "column": "DOC_A", "index": 0 });
    assert!(!refuses::<
        river_db::routes::private::data_streams::replicates::ColumnAssignment,
    >(assignment));
}
