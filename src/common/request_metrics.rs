//! Request volume as a periodic count rather than a line per request.
//!
//! A sync cycle drives hundreds of `/api/ingest` calls a minute and a health probe runs every ten
//! seconds, so logging each one buries the entries an operator is watching for. Individual requests
//! are recorded at DEBUG; this accumulates them and reports one INFO line per interval. Anything a
//! request did wrong keeps its own line at its own level.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static TOTAL: AtomicU64 = AtomicU64::new(0);
static CLIENT_ERRORS: AtomicU64 = AtomicU64::new(0);
static SERVER_ERRORS: AtomicU64 = AtomicU64::new(0);
static TOTAL_MS: AtomicU64 = AtomicU64::new(0);
static MAX_MS: AtomicU64 = AtomicU64::new(0);

/// Count one finished request. Called from the trace layer's response hook.
pub fn record(status: u16, latency_ms: u64) {
    TOTAL.fetch_add(1, Ordering::Relaxed);
    if (400..500).contains(&status) {
        CLIENT_ERRORS.fetch_add(1, Ordering::Relaxed);
    } else if status >= 500 {
        SERVER_ERRORS.fetch_add(1, Ordering::Relaxed);
    }
    TOTAL_MS.fetch_add(latency_ms, Ordering::Relaxed);
    MAX_MS.fetch_max(latency_ms, Ordering::Relaxed);
}

/// Log one line per `interval` for as long as requests arrive, and nothing at all while idle.
///
/// Counters are taken with `swap`, so a request finishing mid-read lands in the next interval
/// rather than being lost or double counted.
pub async fn report_every(interval: Duration) {
    if interval.is_zero() {
        return;
    }
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let total = TOTAL.swap(0, Ordering::Relaxed);
        let client_errors = CLIENT_ERRORS.swap(0, Ordering::Relaxed);
        let server_errors = SERVER_ERRORS.swap(0, Ordering::Relaxed);
        let total_ms = TOTAL_MS.swap(0, Ordering::Relaxed);
        let max_ms = MAX_MS.swap(0, Ordering::Relaxed);
        if total == 0 {
            continue;
        }
        tracing::info!(
            requests = total,
            client_errors,
            server_errors,
            mean_ms = total_ms / total,
            max_ms,
            "Served {total} requests in {}s",
            interval.as_secs()
        );
    }
}
