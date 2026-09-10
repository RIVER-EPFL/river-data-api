//! The wiring assertions on the cache keys: that each handler's key struct is the one carrying
//! its query, so a filter reaches the key rather than being listed by hand and forgotten. That a
//! flattened field separates keys at all is `cache_key.rs`'s own test.

use super::{AggregatesCacheKey, ReadingsCacheKey, SiteAggregatesQuery, SiteReadingsQuery};
use crate::common::cache_key;

fn readings_key(query: serde_json::Value) -> String {
    let query: SiteReadingsQuery = serde_json::from_value(query).expect("the query deserialises");
    cache_key::key_for(
        "readings",
        &ReadingsCacheKey {
            effective_start: chrono::Utc::now(),
            effective_end: None,
            resolved_format: "json",
            query: &query,
        },
    )
}

fn aggregates_key(split: Option<bool>, effective_split: bool) -> String {
    let mut query = serde_json::json!({
        "start": "2026-01-15T00:00:00Z",
        "end": "2026-01-16T00:00:00Z",
    });
    if let Some(split) = split {
        query["split_by_sensor"] = serde_json::json!(split);
    }
    let query: SiteAggregatesQuery = serde_json::from_value(query).expect("the query deserialises");
    cache_key::key_for(
        "aggregates",
        &AggregatesCacheKey {
            resolution: "daily",
            resolved_format: "json",
            effective_split,
            query: &query,
        },
    )
}

#[test]
fn test_a_readings_key_carries_the_parameter_filter() {
    let base = serde_json::json!({ "start": "2026-01-15T00:00:00Z" });
    let mut depth = base.clone();
    depth["parameter_ids"] = serde_json::json!("11111111-1111-1111-1111-111111111111");
    let mut turbidity = base.clone();
    turbidity["parameter_ids"] = serde_json::json!("22222222-2222-2222-2222-222222222222");

    assert_ne!(readings_key(depth.clone()), readings_key(turbidity));
    assert_ne!(readings_key(depth), readings_key(base));
}

#[test]
fn test_a_readings_key_carries_every_annotation_opt_in() {
    let base = serde_json::json!({ "start": "2026-01-15T00:00:00Z" });
    for field in [
        "include_flagged",
        "include_sample_stats",
        "include_curves",
        "include_measurement_type",
        "include_origin",
        "include_withdrawn",
    ] {
        let mut on = base.clone();
        on[field] = serde_json::json!(true);
        assert_ne!(
            readings_key(on),
            readings_key(base.clone()),
            "{field} is absent from the key"
        );
    }
}

#[test]
fn test_an_aggregates_key_carries_the_split() {
    assert_ne!(
        aggregates_key(Some(true), true),
        aggregates_key(Some(false), false)
    );
    assert_ne!(
        aggregates_key(Some(false), false),
        aggregates_key(None, false)
    );
}

/// The bulk formats serve the collapsed body whatever the query asked for, so the applied split is
/// in the key beside the requested one and the two bodies cannot share an entry.
#[test]
fn test_a_requested_split_the_format_refuses_is_a_key_of_its_own() {
    assert_ne!(
        aggregates_key(Some(true), false),
        aggregates_key(Some(true), true)
    );
}
