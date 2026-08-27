//! Gate for tests that need the OpenCPU tool runner (compose service `river-tools-r`).
//!
//! The runner is a container, not a library, so a suite run without it would otherwise fail the
//! whole `tools` binary on connection errors. Tests call `require_runner_or_skip` and return
//! early when it answers false, the same shape as the Keycloak gate.

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
async fn reachable() -> bool {
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

/// Gate a runner-dependent test: true to proceed, false to skip.
///
/// A skip reports as a pass, so `REQUIRE_TOOLS_RUNNER` turns it into a failure for environments
/// that are supposed to have the runner. Left unset locally so a bare `cargo test` still works
/// without the container.
pub async fn require_runner_or_skip(test_name: &str) -> bool {
    if reachable().await {
        return true;
    }
    assert!(
        std::env::var("REQUIRE_TOOLS_RUNNER").is_err(),
        "REQUIRE_TOOLS_RUNNER is set but the tool runner at {} is unreachable, so {test_name} cannot run",
        runner_url()
    );
    eprintln!(
        "skipping {test_name}: the analytical tool runner is unreachable at {}",
        runner_url()
    );
    false
}
