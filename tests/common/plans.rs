//! The pairing-plan lifecycle as the three plan stories drive it: draft, patch, apply and revert,
//! each over the HTTP surface. Apply and revert run as tracked `plan_apply`/`plan_revert` jobs, so
//! a caller that asserts a database fact has to wait for the job rather than for the response.

use axum::Router;
use serde_json::json;

/// Draft a plan over a source system, asserting the server answers with a draft.
pub async fn create_plan(app: &Router, token: &str, source_system: &str) -> serde_json::Value {
    let (status, plan) = super::post_json_parse_with_token(
        app,
        "/api/sync/pairing-plans",
        &json!({ "source_system": source_system }),
        token,
    )
    .await;
    assert_eq!(
        status, 200,
        "create plan for {source_system} ({status}): {plan}"
    );
    assert_eq!(plan["status"], "draft", "a new plan is a draft: {plan}");
    plan
}

/// One debounced PATCH batch, returning the server's snapshot of the plan. The dashboard replaces
/// its local plan with exactly this body, so it has to carry the accumulated state.
pub async fn patch_plan(
    app: &Router,
    token: &str,
    plan_id: &str,
    updates: serde_json::Value,
) -> serde_json::Value {
    let (status, body) = super::patch_plan_with_token(
        app,
        &plan_id.to_string(),
        &json!({ "updates": updates }),
        token,
    )
    .await;
    assert_eq!(status, 200, "PATCH plan ({status}): {body}");
    serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("PATCH response is JSON: {e}\nBody: {body}"))
}

/// The plan's entry for one stream.
#[must_use]
pub fn find_entry<'a>(plan: &'a serde_json::Value, stream_id: &str) -> &'a serde_json::Value {
    plan["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("entries array: {plan}"))
        .iter()
        .find(|e| e["stream_id"] == json!(stream_id))
        .unwrap_or_else(|| panic!("entry for stream {stream_id} missing: {plan}"))
}

/// Post `apply`/`revert` on a plan, wait for its job to reach `completed`, and return the job's
/// `detail.counts`. The pairing, the backfill and the plan's status transition all happen in that
/// job, so a fact read from the database is only true once this has returned.
pub async fn run_plan_action(
    app: &Router,
    token: &str,
    plan_id: &str,
    action: &str,
) -> serde_json::Value {
    let (status, res) =
        super::post_plan_action_parse_with_token(app, &plan_id.to_string(), action, token).await;
    assert_eq!(status, 200, "{action} ({status}): {res}");
    let job_id = res["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{action} returns a job_id: {res}"));
    assert_eq!(
        super::e2e::poll_job(app, token, job_id, 30).await,
        "completed",
        "{action} job completes",
    );
    let (_, job) =
        super::get_json_with_token(app, &format!("/api/reprocessing_jobs/{job_id}"), token).await;
    job["detail"]["counts"].clone()
}
