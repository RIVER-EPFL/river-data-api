//! Gate for tests that need the OpenCPU tool runner (compose service `river-tools-r`).
//!
//! The runner is a container, not a library, so a suite run that does not cover it would otherwise
//! fail the whole `tools` binary on connection errors. Tests call `Service::ToolsRunner.require`
//! and return early when the profile excludes it, the same shape as the Keycloak gate.

use std::time::Duration;

/// Where the runner is, resolved exactly as `test_config()` resolves it so a gated test and the
/// app it builds always talk to the same place.
#[must_use]
pub fn runner_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("TOOLS_RUNNER_URL").unwrap_or_else(|_| "http://localhost:8006/ocpu".to_string())
}

/// Probe the runner once per test binary. The answer cannot change mid-run in a way a test could
/// act on, and probing per test would add a request to every one of them.
pub(crate) async fn reachable() -> bool {
    static PROBE: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
    *PROBE
        .get_or_init(|| async {
            let url = format!(
                "{}/library/riverdata.tools/R/runtime_info/json?auto_unbox=true",
                runner_url()
            );
            let Ok(client) = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
            else {
                return false;
            };
            matches!(
                client.post(&url).json(&serde_json::json!({})).send().await,
                Ok(r) if r.status().is_success()
            )
        })
        .await
}
