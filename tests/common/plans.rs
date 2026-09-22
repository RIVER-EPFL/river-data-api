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

/// Tick every entry of a plan and accept its instrument suggestions, which is what the review does
/// before an apply is allowed.
///
/// Q133 gated the apply on `needs_checking == 0`, and Q195 on every suggested instrument being
/// confirmed, so an untouched plan is refused. A test whose subject is what the apply then does
/// says so here rather than carrying the gates' refusals into its own assertions; the gates
/// themselves are exercised by `apply_is_refused_while_a_row_still_needs_checking` and
/// `an_untouched_plan_is_refused_until_its_suggestions_are_accepted`. A suggestion colliding with
/// an existing instrument's name is left alone, as the review's bulk accept leaves it.
pub async fn acknowledge_plan(app: &Router, token: &str, plan_id: &str) {
    let (status, plan) =
        super::get_json_with_token(app, &format!("/api/sync/pairing-plans/{plan_id}"), token).await;
    assert_eq!(status, 200, "reading the plan to tick it: {plan}");
    let updates: Vec<serde_json::Value> = plan["entries"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter(|e| e["acknowledged"] != json!(true) || is_suggestion(e))
                .map(|e| {
                    let mut update = json!({ "stream_id": e["stream_id"], "acknowledged": true });
                    if is_suggestion(e) {
                        update["instrument_confirmed"] = json!(true);
                    }
                    update
                })
                .collect()
        })
        .unwrap_or_default();
    if updates.is_empty() {
        return;
    }
    let (status, text) = super::patch_json_with_token(
        app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &json!({ "expected_version": plan["version"], "updates": updates }),
        token,
    )
    .await;
    assert_eq!(status, 200, "ticking the plan's rows: {text}");
}

/// An instrument the plan suggests, to create or to attach, that nobody has confirmed, no existing
/// name collides with, and no curve label ties.
fn is_suggestion(entry: &serde_json::Value) -> bool {
    let instrument = &entry["instrument"];
    !instrument.is_null()
        && instrument["confirmed"] != json!(true)
        && instrument["resolved_by"] != json!("ambiguous_label")
        && instrument["name_conflict"].is_null()
}

/// Agree to every instrument the plan would create, which is what an operator does before an apply.
///
/// Registration mints nothing (M172), so a stream naming a curve column that resolves to no
/// instrument reaches the plan as a proposal, and `refuse_unconfirmed_instruments` refuses the
/// apply while one stands. A story about what the apply then does says so here; the refusal itself
/// is exercised by `portal_curve_instrument`.
pub async fn confirm_plan_instruments(app: &Router, token: &str, plan_id: &str) {
    let (status, plan) =
        super::get_json_with_token(app, &format!("/api/sync/pairing-plans/{plan_id}"), token).await;
    assert_eq!(
        status, 200,
        "reading the plan to confirm its instruments: {plan}"
    );
    let updates: Vec<serde_json::Value> = plan["entries"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter(|e| {
                    e["instrument"]["create"] == json!(true)
                        && e["instrument"]["confirmed"] != json!(true)
                })
                .map(|e| json!({ "stream_id": e["stream_id"], "instrument_confirmed": true }))
                .collect()
        })
        .unwrap_or_default();
    if updates.is_empty() {
        return;
    }
    let (status, text) = super::patch_json_with_token(
        app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &json!({ "expected_version": plan["version"], "updates": updates }),
        token,
    )
    .await;
    assert_eq!(status, 200, "confirming the plan's instruments: {text}");
}

/// Attach every curve the plan's source holds to the instrument the plan proposes for its curve
/// column, which is the review's answer for a source carrying one curve column. Fails when the
/// plan proposes no such instrument, since the apply would refuse the plan anyway.
pub async fn attach_held_curves(app: &Router, token: &str, plan_id: &str) {
    let (status, view) = super::get_json_with_token(
        app,
        &format!("/api/sync/pairing-plans/{plan_id}/instruments"),
        token,
    )
    .await;
    assert_eq!(status, 200, "reading the plan's held curves: {view}");
    let held: Vec<serde_json::Value> = view["held_curves"]
        .as_array()
        .map(|curves| curves.iter().map(|c| c["id"].clone()).collect())
        .unwrap_or_default();
    if held.is_empty() {
        return;
    }
    let (status, plan) =
        super::get_json_with_token(app, &format!("/api/sync/pairing-plans/{plan_id}"), token).await;
    assert_eq!(status, 200, "reading the plan's instruments: {plan}");
    let instrument_key = plan["entries"]
        .as_array()
        .and_then(|entries| {
            entries.iter().find(|e| {
                e["action"] == json!("pair")
                    && e["instrument"]["create"] == json!(true)
                    && e["instrument"]["curve_column"].is_string()
            })
        })
        .map(|e| e["instrument"]["source_key"].clone())
        .unwrap_or_else(|| panic!("the plan proposes no instrument for a curve column: {plan}"));
    let attachments: Vec<serde_json::Value> = held
        .iter()
        .map(|id| json!({ "proposal_id": id, "instrument_source_key": instrument_key }))
        .collect();
    let (status, text) = super::patch_json_with_token(
        app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &json!({ "expected_version": plan["version"], "held_curves": attachments }),
        token,
    )
    .await;
    assert_eq!(status, 200, "attaching the held curves: {text}");
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
    if action == "apply" {
        acknowledge_plan(app, token, plan_id).await;
    }
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
