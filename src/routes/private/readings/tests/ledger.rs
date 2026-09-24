use chrono::{TimeZone, Utc};
use uuid::Uuid;

use super::{arrivals, pairings};
use crate::routes::private::data_streams;
use crate::routes::private::readings::models::RawRow;

fn stream(id: Uuid, paired_at: Option<chrono::DateTime<Utc>>) -> data_streams::Model {
    let at = Utc
        .with_ymd_and_hms(2024, 9, 20, 0, 0, 0)
        .unwrap()
        .fixed_offset();
    data_streams::Model {
        id,
        source_system: "cnet".to_string(),
        source_key: "FP11/DO".to_string(),
        source_name: None,
        source_path: None,
        metadata: serde_json::json!({}),
        site_parameter_id: Some(Uuid::from_u128(7)),
        sensor_id: None,
        measurement_type: None,
        is_active: true,
        discovered_at: at,
        paired_at: paired_at.map(|t| t.fixed_offset()),
        last_data_time: None,
        last_window_digest: None,
        pairing_plan_id: None,
        created_at: at,
        updated_at: at,
        replicates: None,
    }
}

fn raw(
    stream_id: Uuid,
    replicate_index: i16,
    ingested_at: Option<chrono::DateTime<Utc>>,
) -> RawRow {
    RawRow {
        stream_id,
        replicate_index,
        site_id: None,
        parameter_id: None,
        raw_value: 1.0,
        calibrated_value: None,
        sensor_id: None,
        calibration_id: None,
        standard_curve_id: None,
        deployment_id: None,
        measurement_type: Some("spot".to_string()),
        is_flagged: None,
        flag_reason: None,
        sample_id: None,
        collection_event_id: None,
        withdrawn_at: None,
        unverified: None,
        withdrawn_reason: None,
        ingested_at,
        provenance_kind: Some("sync".to_string()),
        provenance: None,
        derived_version_id: None,
        label: None,
        notes: None,
        created_by: None,
    }
}

#[test]
fn test_arrivals_one_row_per_stream_and_arrival_time() {
    let id = Uuid::from_u128(1);
    let first = Utc.with_ymd_and_hms(2024, 9, 22, 6, 57, 0).unwrap();
    let later = Utc.with_ymd_and_hms(2024, 9, 23, 9, 0, 0).unwrap();
    let rows = [
        raw(id, 0, Some(first)),
        raw(id, 1, Some(first)),
        raw(id, 2, Some(later)),
        raw(id, 3, None),
    ];
    let entries = arrivals(&rows, &[stream(id, None)]);
    assert_eq!(entries.len(), 2);
    let e = &entries[0];
    assert_eq!(e.source, "arrival");
    assert_eq!(e.at, first);
    assert_eq!(e.id, id);
    let new = e.new.as_ref().unwrap();
    assert_eq!(new["origin"], "sync");
    assert_eq!(new["source_system"], "cnet");
    assert_eq!(new["source_key"], "FP11/DO");
    assert_eq!(new["replicates"], serde_json::json!([0, 1]));
    assert_eq!(
        entries[1].new.as_ref().unwrap()["replicates"],
        serde_json::json!([2])
    );
}

#[test]
fn test_arrivals_empty_without_the_stream() {
    let rows = [raw(Uuid::from_u128(1), 0, Some(Utc::now()))];
    assert!(arrivals(&rows, &[]).is_empty());
}

#[test]
fn test_pairings_names_stream_and_slot() {
    let id = Uuid::from_u128(1);
    let paired = Utc.with_ymd_and_hms(2024, 9, 22, 8, 8, 0).unwrap();
    let site = Uuid::from_u128(2);
    let parameter = Uuid::from_u128(3);
    let entries = pairings(&[stream(id, Some(paired))], Some(site), Some(parameter));
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e.source, "pairing");
    assert_eq!(e.at, paired);
    let new = e.new.as_ref().unwrap();
    assert_eq!(new["source_key"], "FP11/DO");
    assert_eq!(new["site_parameter_id"], Uuid::from_u128(7).to_string());
    assert_eq!(new["site_id"], site.to_string());
    assert_eq!(new["parameter_id"], parameter.to_string());
}

#[test]
fn test_pairings_skip_an_unpaired_stream() {
    assert!(pairings(&[stream(Uuid::from_u128(1), None)], None, None).is_empty());
}
