use chrono::{DateTime, Utc};
use river_data_core::models::{
    NoteUpsert, RegisterStreamRequest as CoreRegisterStreamRequest, ReplicateSpec as CoreSpec,
    SensorUpsert, StandardCurveUpsert,
};
use serde_json::json;
use uuid::Uuid;

use river_db::routes::private::data_streams::models::{
    ColumnAssignment, RegisterStreamRequest, ReplicateSpec,
};
use river_db::routes::private::notes::models::NoteItem;
use river_db::routes::private::sensors::models::ProposeInstrumentsRequest;
use river_db::routes::private::standard_curves::models::RegisterStandardCurveRequest;

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

/// Core's struct as it goes on the wire, then read back as the API reads it.
fn through<S: serde::Serialize, D: serde::de::DeserializeOwned>(sent: &S) -> D {
    let json = serde_json::to_value(sent).expect("the sender serialises");
    serde_json::from_value(json.clone())
        .unwrap_or_else(|e| panic!("the receiver refuses what the sender sends: {e}\n{json:#}"))
}

fn refuses<D: serde::de::DeserializeOwned>(mut json: serde_json::Value) -> bool {
    json["a_field_the_sender_gained"] = json!(1);
    serde_json::from_value::<D>(json).is_err()
}

/// A client sends what core declares; the API adds only `source_system`, so everything the client
/// can say about an instrument has to arrive through core's struct, cadence included.
#[test]
fn an_instrument_proposal_arrives_whole() {
    let sent = SensorUpsert {
        source_key: "sensor_inventory:62".to_string(),
        name: "DOC corr".to_string(),
        serial_number: Some("SN-62".to_string()),
        manufacturer: Some("Shimadzu".to_string()),
        model: Some("TOC-L".to_string()),
        notes: Some("lab bench".to_string()),
        is_lab_instrument: true,
        data_frequency: Some("low".to_string()),
        metadata: Some(json!({ "bench": 2 })),
    };
    let body = json!({ "source_system": "cnet", "instruments": [sent] });
    let got: ProposeInstrumentsRequest = serde_json::from_value(body).expect("the API reads it");

    assert_eq!(got.source_system, "cnet");
    let [got] = got.instruments.as_slice() else {
        panic!("one instrument arrives");
    };
    assert_eq!(got.source_key, sent.source_key);
    assert_eq!(got.name, sent.name);
    assert_eq!(got.serial_number, sent.serial_number);
    assert_eq!(got.manufacturer, sent.manufacturer);
    assert_eq!(got.model, sent.model);
    assert_eq!(got.notes, sent.notes);
    assert_eq!(got.is_lab_instrument, sent.is_lab_instrument);
    assert_eq!(got.data_frequency.as_deref(), Some("low"));
    assert_eq!(got.metadata, sent.metadata);
}

/// A curve's `notes` is one of the three fields that had drifted: the API held the column and core
/// could not send it.
#[test]
fn a_standard_curve_registration_carries_its_notes() {
    let sent = StandardCurveUpsert {
        source_key: "standard_curves:17".to_string(),
        instrument_label: "DOC corr".to_string(),
        slope: 1.5,
        intercept: -0.25,
        r_squared: Some(0.998),
        name: Some("2026-01 DOC".to_string()),
        fitted_on: Some(at("2026-01-15T00:00:00Z").date_naive()),
        notes: Some("三 standards, r2 reported by the portal".to_string()),
    };
    let mut body = serde_json::to_value(&sent).expect("the upsert serialises");
    body["source_system"] = json!("cnet");
    let got: RegisterStandardCurveRequest = serde_json::from_value(body).expect("the API reads it");

    assert_eq!(got.source_system, "cnet");
    assert_eq!(got.curve.source_key, sent.source_key);
    assert_eq!(got.curve.instrument_label, sent.instrument_label);
    assert_eq!(got.curve.fitted_on, sent.fitted_on);
    assert_eq!(got.curve.notes, sent.notes);
}

/// The stream registration is core's struct behind a shim that keeps `metadata` optional.
#[test]
fn a_stream_registration_arrives_whole_and_may_omit_metadata() {
    let sent = CoreRegisterStreamRequest {
        source_system: "cnet".to_string(),
        source_key: "FP1:DOC_avg_ppb:reps".to_string(),
        source_name: Some("DOC".to_string()),
        source_path: Some("cnet/FP1".to_string()),
        metadata: json!({ "station": "FP1" }),
        measurement_type: Some("spot".to_string()),
        sensor_id: Some(Uuid::from_u128(5)),
        replicates: Some(CoreSpec {
            source_columns: vec!["DOC_A".to_string(), "DOC_B".to_string()],
            portal_mean_column: Some("DOC_avg_ppb".to_string()),
            portal_sd_column: Some("DOC_sd_ppb".to_string()),
            curve_ref_column: Some("doc_std_curve_id".to_string()),
            calc: Some("calcMean".to_string()),
            sd_estimator: Some("population".to_string()),
        }),
        decimal_places: Some(2),
        instrument_granularity: None,
    };
    let RegisterStreamRequest(got) = through(&sent);
    assert_eq!(got.metadata, sent.metadata);
    assert_eq!(got.decimal_places, sent.decimal_places);
    assert_eq!(
        got.replicates.expect("the declaration arrives").calc,
        Some("calcMean".to_string())
    );

    let RegisterStreamRequest(bare) = serde_json::from_value(json!({
        "source_system": "vaisala", "source_key": "1270",
    }))
    .expect("the API reads a registration without metadata");
    assert_eq!(bare.metadata, json!({}));
}

/// Core requires `verified`; this route has always defaulted it.
#[test]
fn a_note_may_omit_the_verified_flag() {
    let sent = NoteUpsert {
        source_key: "notes:1".to_string(),
        site_name: "FP1".to_string(),
        text: "bank collapsed upstream".to_string(),
        verified: true,
    };
    let got: NoteItem = through(&sent);
    assert_eq!(got.site_name, sent.site_name);
    assert!(got.verified);

    let bare: NoteItem = serde_json::from_value(json!({
        "source_key": "notes:2", "site_name": "FP1", "text": "x",
    }))
    .expect("the API reads a note without the flag");
    assert!(!bare.verified);
}

/// The declaration is the caller's and the index mapping is the server's. The stored spec carries
/// both, and a registration cannot express an assignment at all.
#[test]
fn a_replicate_declaration_carries_no_assignments() {
    let stored = ReplicateSpec {
        declared: CoreSpec {
            source_columns: vec!["DOC_A".to_string(), "DOC_B".to_string()],
            portal_mean_column: None,
            portal_sd_column: None,
            curve_ref_column: None,
            calc: None,
            sd_estimator: None,
        },
        assignments: vec![ColumnAssignment {
            column: "DOC_A".to_string(),
            index: 0,
            retired: false,
        }],
    };
    let json = serde_json::to_value(&stored).expect("the stored spec serialises");
    assert_eq!(
        json["source_columns"][1], "DOC_B",
        "the declaration stays flat in stream metadata, where the sync clients read it"
    );

    let declared: CoreSpec = serde_json::from_value(json).expect("core reads the stored spec");
    assert_eq!(declared.source_columns, stored.declared.source_columns);
}

/// Carrying a core struct inside a request body must not lose its refusal: unknown fields are
/// refused, not dropped, on every write-path request that carried the attribute.
#[test]
fn the_receivers_that_refuse_an_unknown_field() {
    assert!(refuses::<ProposeInstrumentsRequest>(json!({
        "source_system": "cnet", "instruments": [],
    })));
    assert!(
        serde_json::from_value::<ProposeInstrumentsRequest>(json!({
            "source_system": "cnet",
            "instruments": [{
                "source_key": "sensor_inventory:62", "name": "DOC corr",
                "is_lab_instrument": false, "a_field_the_sender_gained": 1,
            }],
        }))
        .is_err(),
        "an unknown field inside a proposed instrument is refused, not dropped"
    );
    assert!(refuses::<NoteItem>(json!({
        "source_key": "notes:1", "site_name": "FP1", "text": "x", "verified": false,
    })));

    // The three that do not refuse, on either side. A field a sender gains here is dropped in
    // silence, which is why one type declaring the fields is the guard rather than the deserialize.
    assert!(!refuses::<RegisterStreamRequest>(json!({
        "source_system": "cnet", "source_key": "FP1:DOC", "metadata": {},
    })));
    assert!(!refuses::<RegisterStandardCurveRequest>(json!({
        "source_system": "cnet", "source_key": "standard_curves:17",
        "instrument_label": "DOC corr", "slope": 1.0, "intercept": 0.0,
    })));
    assert!(!refuses::<ColumnAssignment>(
        json!({ "column": "DOC_A", "index": 0 })
    ));
}
