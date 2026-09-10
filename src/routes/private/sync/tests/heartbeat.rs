use super::*;

/// Scenario: an operator shortens `SYNC_SESSION_TOKEN_TTL_SECS`.
///
/// Expected behaviour: the cache window shortens with it and stays strictly inside the token's
/// lifetime. A fixed window wider than a configured lifetime would have the heartbeat hand back
/// tokens the database had already expired, 401 its own next call, and churn the service
/// through re-enrollment.
#[test]
fn the_cache_window_stays_inside_the_token_lifetime() {
    let window = |ttl: u64| (ttl as f64 * CACHE_FRACTION_OF_TTL) as u64;
    for ttl in [60, 300, 600, DEFAULT_SESSION_TOKEN_TTL_SECS, 3600] {
        assert!(
            window(ttl) < ttl,
            "a {ttl}s token must outlive its {}s cache entry",
            window(ttl)
        );
    }
    assert_eq!(window(DEFAULT_SESSION_TOKEN_TTL_SECS), 720);
}
