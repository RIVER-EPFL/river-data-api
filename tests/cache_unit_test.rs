//! Unit tests for cache module.
//!
//! Run with: cargo test --test cache_unit_test

use river_db::routes::cache;

#[test]
fn cache_key_builds_correctly() {
    // Basic key building
    assert_eq!(cache::cache_key("readings", &[]), "readings");
    assert_eq!(
        cache::cache_key("readings", &["station", "2025-01-01", "json"]),
        "readings:station:2025-01-01:json"
    );

    // Empty components preserved (ensures query uniqueness)
    assert_ne!(
        cache::cache_key("readings", &["station", "", "json"]),
        cache::cache_key("readings", &["station", "json"])
    );
}
