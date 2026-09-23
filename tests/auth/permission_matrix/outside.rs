//! The outside half of the table: every row a confined caller can pass the gate of, issued against
//! a project that caller was never granted.
//!
//! Scenario: the seed project beside a second one holding a site, a slot, a paired stream with a
//! reading, a deployed instrument with a calibration and a curve, an alarm event, a visit, a job, a
//! tool run, a review hold and a row of each project-bound entity.
//!
//! Expected behaviour: a granted member and a project-scoped token are refused every row of the
//! second project that names it, and a listing they are answered carries none of its rows.

use serde_json::{Value, json};
use serial_test::serial;

use super::{
    Caller, Level, MISSING_ID, OTHER_PROJECT_ID, OTHER_SITE_ID, OTHER_SLOT_ID, OTHER_STREAM_ID,
    Outcome, Route, entities, expected, table,
};
use crate::common::fixtures::{GLOBAL_PARAM_TEMP_ID, PROJECT_ID};
use crate::common::keycloak::{
    build_test_app_with_keycloak_admin, ensure_realm_user, get_keycloak_jwt, grant_project,
    keycloak_reachable, keycloak_user_id,
};

const OTHER_SENSOR_ID: &str = "00000000-0000-4000-a000-000000000095";
const OTHER_DEPLOYMENT_ID: &str = "00000000-0000-4000-a000-000000000094";
const OTHER_CALIBRATION_ID: &str = "00000000-0000-4000-a000-000000000093";
const OTHER_CURVE_ID: &str = "00000000-0000-4000-a000-000000000092";
const OTHER_ALARM_EVENT_ID: &str = "00000000-0000-4000-a000-000000000091";
const OTHER_VISIT_ID: &str = "00000000-0000-4000-a000-000000000090";
const OTHER_JOB_ID: &str = "00000000-0000-4000-a000-00000000008f";
const OTHER_TOOL_RUN_ID: &str = "00000000-0000-4000-a000-00000000008e";
const OTHER_HOLD_ID: &str = "00000000-0000-4000-a000-00000000008d";
const OTHER_SUBPROJECT_ID: &str = "00000000-0000-4000-a000-00000000008c";
const OTHER_NOTE_ID: &str = "00000000-0000-4000-a000-00000000008b";
const OTHER_ANNOTATION_ID: &str = "00000000-0000-4000-a000-00000000008a";
const OTHER_THRESHOLD_ID: &str = "00000000-0000-4000-a000-000000000089";
const OTHER_PROPOSAL_ID: &str = "00000000-0000-4000-a000-000000000088";
const OTHER_ENTRY_HOLD_ID: &str = "00000000-0000-4000-a000-000000000087";
const OTHER_DECIDED_HOLD_ID: &str = "00000000-0000-4000-a000-000000000086";
const OTHER_CALC_ID: &str = "00000000-0000-4000-a000-000000000084";
const OTHER_CALC_VERSION_ID: &str = "00000000-0000-4000-a000-000000000083";
const OTHER_INPUT_ID: &str = "00000000-0000-4000-a000-000000000082";
const OTHER_INPUT_SLOT_ID: &str = "00000000-0000-4000-a000-000000000081";
const OTHER_OUTPUT_ID: &str = "00000000-0000-4000-a000-000000000080";
const OTHER_SESSION_ID: &str = "00000000-0000-4000-a000-00000000007f";
/// An instrument deployed nowhere, which any caller may name.
const INVENTORY_SENSOR_ID: &str = "00000000-0000-4000-a000-000000000085";
const OTHER_CODE: &str = "OUTSIDE";
const OTHER_TIME: &str = "2025-01-01T00:00:00Z";

/// The second project's ids a listing must not carry. The instrument is left out: the inventory is
/// shared, so a sensor deployed only in the second project is listed to everyone.
const OTHER_ROWS: [&str; 20] = [
    OTHER_PROJECT_ID,
    OTHER_SITE_ID,
    OTHER_SLOT_ID,
    OTHER_STREAM_ID,
    OTHER_DEPLOYMENT_ID,
    OTHER_CALIBRATION_ID,
    OTHER_CURVE_ID,
    OTHER_ALARM_EVENT_ID,
    OTHER_VISIT_ID,
    OTHER_JOB_ID,
    OTHER_TOOL_RUN_ID,
    OTHER_HOLD_ID,
    OTHER_SUBPROJECT_ID,
    OTHER_NOTE_ID,
    OTHER_ANNOTATION_ID,
    OTHER_THRESHOLD_ID,
    OTHER_PROPOSAL_ID,
    OTHER_ENTRY_HOLD_ID,
    OTHER_DECIDED_HOLD_ID,
    OTHER_INPUT_SLOT_ID,
];

/// Rows a confined caller passes the gate of that name no project, with the reason.
const NOT_PROJECT_BOUND: [(&str, &str); 20] = [
    ("GET /api/version", "the build"),
    ("GET /api/me", "the caller's own identity"),
    (
        "GET /api/meteoswiss/stations",
        "the national station catalog",
    ),
    ("GET /api/tools", "the calculation catalog"),
    (
        "GET /api/derived_parameters/{id}/dependents",
        "the calculation catalog",
    ),
    (
        "GET /api/parameter_groups/{id}/definition",
        "the parameter catalog",
    ),
    ("GET /api/schedules", "a job cadence names no project"),
    (
        "GET /api/schedules/{job_name}",
        "a job cadence names no project",
    ),
    (
        "GET /api/schedules/{job_name}/audit",
        "a job cadence names no project",
    ),
    (
        "GET /api/schedules/runnable",
        "a job cadence names no project",
    ),
    (
        "PATCH /api/schedules/{job_name}",
        "a job cadence names no project",
    ),
    (
        "POST /api/schedules/{job_name}/run_now",
        "a job cadence names no project",
    ),
    (
        "GET /api/notifications/me",
        "the caller's own subscriber record",
    ),
    (
        "PATCH /api/notifications/me",
        "the caller's own subscriber record",
    ),
    (
        "GET /api/notifications/me/push",
        "the caller's own subscriber record",
    ),
    (
        "POST /api/notifications/me/push/ping",
        "the caller's own subscriber record",
    ),
    ("GET /api/notifications/channels", "the delivery channels"),
    (
        "GET /api/sync/unpaired-summary",
        "an unpaired stream belongs to no project",
    ),
    (
        "POST /api/actions/reconcile_alarms",
        "recomputes the open-alarm set the sweeper recomputes anyway (common/scope.rs)",
    ),
    (
        "PUT /api/notifications/me/subscriptions",
        "delivery admits only the subscriber's live grants (notifications/service.rs)",
    ),
];

/// Rows whose outside half is asserted elsewhere: the edits by
/// `scope_confinement_denies_another_projects_row`, which builds the committed edit a rollback
/// needs, and the event stream, whose confinement is per frame, by
/// `tests/events/sse_event_stream.rs`, `a_scoped_token_hears_only_its_own_projects_sites`.
const PROBED_ELSEWHERE: [&str; 6] = [
    "GET /api/events",
    "POST /api/readings/edits/preview",
    "POST /api/readings/edits",
    "POST /api/readings/edits/{id}/rollback",
    "POST /api/readings/edits/sets/{set_id}/rollback",
    "POST /api/readings/edits/inspect",
];

/// Project-bound rows with no outside probe yet. Each one is a row whose confinement nothing
/// asserts, so the list only shrinks.
const UNPROBED: [&str; 0] = [];

/// Project-bound rows whose outside probe is known to reach the other project, each under the item
/// that fixes it. The probe is issued and its answer printed, not asserted; the fix drops the entry.
const KNOWN_LEAKS: [(&str, &str); 2] = [
    ("GET /api/tool_scripts/{id}/version_ledger", "B563"),
    (
        "POST /api/actions/derived_parameters/{id}/recompute",
        "B564",
    ),
];

/// How a confined caller is answered when a request names another project's row.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Confined {
    /// 403, or 404 where the row filter hides the row.
    Refused,
    /// 403 alone, where the request's other target is absent and a 404 would not be confinement.
    Forbidden,
    /// Refused, or answered with none of the other project's rows in the body.
    Filtered,
    /// Answered with each named row refused inside the body, or none of the other project's rows
    /// counted in it, which carries this.
    Declined(&'static str),
    /// Answered with this status and a body carrying this, as if the named row were not there.
    Answered(u16, &'static str),
}

struct Probe {
    row: String,
    method: &'static str,
    path: String,
    body: Option<Value>,
    confined: Confined,
}

/// A row's key: `METHOD declared`, with a CRUD row keyed by its entity's path.
fn row_key(route: &Route) -> String {
    if route.declared.starts_with("crud:") {
        return format!(
            "{} {}",
            route.method,
            route.path.replace(MISSING_ID, "{id}")
        );
    }
    format!("{} {}", route.method, route.declared)
}

/// The CRUD rows over an entity with no project dimension, keyed as [`row_key`] keys them.
fn global_crud_rows() -> Vec<String> {
    entities()
        .into_iter()
        .filter(|e| e.crud == super::CrudScope::Global)
        .flat_map(|e| {
            [
                format!("GET /api/{}", e.name),
                format!("POST /api/{}", e.name),
                format!("DELETE /api/{}/{{id}}", e.name),
            ]
        })
        .collect()
}

const RESTRICTED: [Caller; 4] = [
    Caller::ScopedToken,
    Caller::Member(Level::Intern),
    Caller::Member(Level::River),
    Caller::Member(Level::Manager),
];

/// The rows a confined caller can pass the gate of, by [`row_key`].
fn confinable_rows() -> Vec<String> {
    let mut out: Vec<String> = table()
        .0
        .iter()
        .filter(|r| {
            RESTRICTED
                .iter()
                .any(|c| expected(r, *c) == Outcome::Allowed)
        })
        .map(row_key)
        .collect();
    out.sort();
    out.dedup();
    out
}

fn probes() -> Vec<Probe> {
    let mut out = Vec::new();
    let mut add = |method: &'static str,
                   declared: &str,
                   path: String,
                   body: Option<Value>,
                   confined: Confined| {
        out.push(Probe {
            row: format!("{method} {declared}"),
            method,
            path,
            body,
            confined,
        });
    };
    let refused = Confined::Refused;
    let filtered = Confined::Filtered;
    let range = "start=2024-12-31T00:00:00Z&end=2025-01-02T00:00:00Z";

    // --- Addressed by site ---
    for tail in [
        "readings",
        "status_events",
        "alarms",
        "annotations",
        "export/summary",
        "export/replicates",
        "export/sensor-vs-grab",
        "statistics",
        "sensor_identity",
        "last_curve",
        "parameters",
        "detail",
    ] {
        add(
            "GET",
            &format!("/api/sites/{{site_id}}/{tail}"),
            format!(
                "/api/sites/{OTHER_SITE_ID}/{tail}?{range}&parameter_id={GLOBAL_PARAM_TEMP_ID}"
            ),
            None,
            refused,
        );
    }
    add(
        "GET",
        "/api/sites/{site_id}/aggregates/{resolution}",
        format!("/api/sites/{OTHER_SITE_ID}/aggregates/hourly?{range}"),
        None,
        refused,
    );
    add(
        "GET",
        "/api/sites/{id}/visits",
        format!("/api/sites/{OTHER_SITE_ID}/visits"),
        None,
        refused,
    );
    for (tail, body) in [
        ("parameter_groups", json!({ "group_id": MISSING_ID })),
        ("calculations", json!({ "calculation_id": MISSING_ID })),
    ] {
        add(
            "POST",
            &format!("/api/sites/{{site_id}}/{tail}"),
            format!("/api/sites/{OTHER_SITE_ID}/{tail}"),
            Some(body),
            Confined::Forbidden,
        );
    }
    add(
        "GET",
        "/api/projects/{project_id}/sites",
        format!("/api/projects/{OTHER_PROJECT_ID}/sites"),
        None,
        filtered,
    );
    add(
        "GET",
        "/api/visits",
        format!("/api/visits?site_id={OTHER_SITE_ID}"),
        None,
        filtered,
    );
    add(
        "GET",
        "/api/me/sites",
        "/api/me/sites".into(),
        None,
        filtered,
    );
    add(
        "GET",
        "/api/search",
        "/api/search?q=Outside".into(),
        None,
        filtered,
    );
    add(
        "POST",
        "/api/readings/seasonal_check",
        "/api/readings/seasonal_check".into(),
        Some(json!({
            "site_id": OTHER_SITE_ID,
            "time": OTHER_TIME,
            "values": [{ "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 4.0 }],
        })),
        refused,
    );

    // --- Addressed by stream or reading ---
    for tail in ["stats", "preview", "receipts"] {
        add(
            "GET",
            &format!("/api/streams/{{id}}/{tail}"),
            format!("/api/streams/{OTHER_STREAM_ID}/{tail}"),
            None,
            refused,
        );
    }
    for tail in ["provenance", "ledger", "decisions", "replay"] {
        add(
            "GET",
            &format!("/api/readings/{tail}"),
            format!("/api/readings/{tail}?stream_id={OTHER_STREAM_ID}&time={OTHER_TIME}"),
            None,
            refused,
        );
    }
    let key = json!([{ "site_id": OTHER_SITE_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": OTHER_TIME }]);
    for tail in ["flag", "unflag"] {
        add(
            "PATCH",
            &format!("/api/readings/{tail}"),
            format!("/api/readings/{tail}"),
            Some(json!({ "readings": key, "reason": "not mine" })),
            refused,
        );
    }
    for tail in ["flag_range", "unflag_range"] {
        add(
            "PATCH",
            &format!("/api/readings/{tail}"),
            format!("/api/readings/{tail}"),
            Some(json!({
                "site_id": OTHER_SITE_ID,
                "parameter_id": GLOBAL_PARAM_TEMP_ID,
                "start_time": "2024-12-31T00:00:00Z",
                "end_time": "2025-01-02T00:00:00Z",
                "reason": "not mine",
            })),
            refused,
        );
    }
    add(
        "POST",
        "/api/readings/batch",
        "/api/readings/batch".into(),
        Some(json!({
            "readings": [{ "site_id": OTHER_SITE_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": "2025-01-01T01:00:00Z", "raw_value": 5.0 }],
        })),
        refused,
    );
    add(
        "POST",
        "/api/ingest",
        "/api/ingest".into(),
        Some(json!({
            "stream_id": OTHER_STREAM_ID,
            "readings": [{ "time": "2025-01-01T01:00:00Z", "raw_value": 5.0 }],
        })),
        refused,
    );

    // --- Addressed by calculation or upload ---
    add(
        "GET",
        "/api/calculations/sites",
        "/api/calculations/sites".into(),
        None,
        filtered,
    );
    add(
        "GET",
        "/api/tool_scripts/{id}/version_ledger",
        format!("/api/tool_scripts/{OTHER_CALC_ID}/version_ledger"),
        None,
        Confined::Declined("\"readings\":0"),
    );
    add(
        "POST",
        "/api/actions/derived_parameters/{id}/recompute",
        format!("/api/actions/derived_parameters/{OTHER_CALC_ID}/recompute"),
        None,
        refused,
    );
    add(
        "POST",
        "/api/readings/import_csv/chunk",
        "/api/readings/import_csv/chunk".into(),
        Some(json!({ "session_id": OTHER_SESSION_ID, "chunk": "2025-01-01T00:00:00Z,1\n" })),
        Confined::Answered(400, "not found"),
    );

    // --- Addressed by instrument ---
    for tail in ["readings", "deployment_bands", "curve_usage"] {
        add(
            "GET",
            &format!("/api/sensors/{{id}}/{tail}"),
            format!("/api/sensors/{OTHER_SENSOR_ID}/{tail}?{range}"),
            None,
            filtered,
        );
    }
    add(
        "GET",
        "/api/sensors/last_used",
        format!("/api/sensors/last_used?parameter_ids={GLOBAL_PARAM_TEMP_ID}"),
        None,
        filtered,
    );
    add(
        "GET",
        "/api/sensors/{sensor_id}/adopt_suggestions",
        format!("/api/sensors/{OTHER_SENSOR_ID}/adopt_suggestions"),
        None,
        filtered,
    );
    add(
        "POST",
        "/api/sensors/{sensor_id}/adopt",
        format!("/api/sensors/{OTHER_SENSOR_ID}/adopt"),
        Some(json!({
            "site_id": OTHER_SITE_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "deployed_from": OTHER_TIME,
        })),
        refused,
    );
    add(
        "POST",
        "/api/sensors/retag_frequency",
        "/api/sensors/retag_frequency".into(),
        Some(json!({ "sensor_ids": [OTHER_SENSOR_ID], "data_frequency": "low" })),
        refused,
    );
    add(
        "POST",
        "/api/actions/reprocess",
        "/api/actions/reprocess".into(),
        Some(json!({ "sensor_id": OTHER_SENSOR_ID })),
        refused,
    );
    add(
        "POST",
        "/api/actions/rollback_deployment",
        "/api/actions/rollback_deployment".into(),
        Some(json!({ "deployment_id": OTHER_DEPLOYMENT_ID })),
        refused,
    );
    add(
        "POST",
        "/api/actions/backfill_attribution",
        "/api/actions/backfill_attribution".into(),
        Some(json!({ "deployment_ids": [OTHER_DEPLOYMENT_ID] })),
        refused,
    );
    add(
        "GET",
        "/api/sensor_calibrations/{id}/window",
        format!("/api/sensor_calibrations/{OTHER_CALIBRATION_ID}/window"),
        None,
        refused,
    );
    add(
        "POST",
        "/api/actions/sensor_calibrations/{id}/recalculate",
        format!("/api/actions/sensor_calibrations/{OTHER_CALIBRATION_ID}/recalculate"),
        Some(json!({})),
        refused,
    );
    for tail in ["retire", "unretire"] {
        add(
            "POST",
            &format!("/api/sensor_calibrations/{{id}}/{tail}"),
            format!("/api/sensor_calibrations/{OTHER_CALIBRATION_ID}/{tail}"),
            Some(json!({})),
            refused,
        );
        add(
            "POST",
            &format!("/api/standard_curves/{{id}}/{tail}"),
            format!("/api/standard_curves/{OTHER_CURVE_ID}/{tail}"),
            Some(json!({})),
            refused,
        );
    }
    add(
        "GET",
        "/api/standard_curves/{id}/usage",
        format!("/api/standard_curves/{OTHER_CURVE_ID}/usage"),
        None,
        filtered,
    );
    for tail in ["backfill_candidates", "calibration_candidates"] {
        add(
            "GET",
            &format!("/api/actions/{tail}"),
            format!("/api/actions/{tail}"),
            None,
            filtered,
        );
    }

    // --- Alarms ---
    for method in ["POST", "DELETE"] {
        add(
            method,
            "/api/alarms/{event_id}/acknowledge",
            format!("/api/alarms/{OTHER_ALARM_EVENT_ID}/acknowledge"),
            (method == "POST").then(|| json!({})),
            refused,
        );
    }
    for tail in ["active", "summary", "events", "thresholds"] {
        add(
            "GET",
            &format!("/api/alarms/{tail}"),
            format!("/api/alarms/{tail}?{range}"),
            None,
            filtered,
        );
    }

    // --- Visits, jobs, tool runs, review holds ---
    add(
        "GET",
        "/api/collection_events/{id}/detail",
        format!("/api/collection_events/{OTHER_VISIT_ID}/detail"),
        None,
        refused,
    );
    for tail in ["preview", "recompute"] {
        add(
            "POST",
            &format!("/api/collection_events/{{id}}/{tail}"),
            format!("/api/collection_events/{OTHER_VISIT_ID}/{tail}"),
            Some(json!({})),
            refused,
        );
    }
    add(
        "GET",
        "/api/reprocessing_jobs/{id}/logs",
        format!("/api/reprocessing_jobs/{OTHER_JOB_ID}/logs"),
        None,
        refused,
    );
    for tail in ["rerun", "cancel"] {
        add(
            "POST",
            &format!("/api/reprocessing_jobs/{{id}}/{tail}"),
            format!("/api/reprocessing_jobs/{OTHER_JOB_ID}/{tail}"),
            Some(json!({})),
            refused,
        );
    }
    for tail in ["reload", "trace"] {
        add(
            "GET",
            &format!("/api/tool_runs/{{id}}/{tail}"),
            format!("/api/tool_runs/{OTHER_TOOL_RUN_ID}/{tail}"),
            None,
            refused,
        );
    }
    add(
        "GET",
        "/api/sync/replicate_audit_holds",
        "/api/sync/replicate_audit_holds".into(),
        None,
        filtered,
    );
    add(
        "GET",
        "/api/sync/replicate_audit_holds/{id}/reject_preview",
        format!("/api/sync/replicate_audit_holds/{OTHER_ENTRY_HOLD_ID}/reject_preview"),
        None,
        refused,
    );
    for tail in [
        "accept_correction",
        "accept_identity",
        "dismiss_finding",
        "release_brake",
        "reopen",
        "resolve",
    ] {
        let hold = if tail == "reopen" {
            OTHER_DECIDED_HOLD_ID
        } else {
            OTHER_HOLD_ID
        };
        add(
            "POST",
            &format!("/api/sync/replicate_audit_holds/{{id}}/{tail}"),
            format!("/api/sync/replicate_audit_holds/{hold}/{tail}"),
            Some(json!({ "mode": "accept", "reason": "not mine" })),
            refused,
        );
    }
    add(
        "POST",
        "/api/actions/invalidate_public_config/{code}",
        format!("/api/actions/invalidate_public_config/{OTHER_CODE}"),
        Some(json!({})),
        refused,
    );

    // --- Named in the body ---
    let visit = json!({ "site_id": OTHER_SITE_ID, "collected_at": OTHER_TIME });
    add(
        "POST",
        "/api/collection_events/stage",
        "/api/collection_events/stage".into(),
        Some(visit.clone()),
        refused,
    );
    add(
        "POST",
        "/api/collection_events/stage_many",
        "/api/collection_events/stage_many".into(),
        Some(json!({ "visits": [visit] })),
        refused,
    );
    add(
        "POST",
        "/api/collection_events/preview",
        "/api/collection_events/preview".into(),
        Some(json!({ "site_id": OTHER_SITE_ID, "collected_at": OTHER_TIME, "staged": [] })),
        refused,
    );
    add(
        "POST",
        "/api/grab_samples",
        "/api/grab_samples".into(),
        Some(json!({
            "site_id": OTHER_SITE_ID,
            "readings": [{ "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 5.0, "time": OTHER_TIME }],
        })),
        refused,
    );
    add(
        "POST",
        "/api/ingest/status_events",
        "/api/ingest/status_events".into(),
        Some(
            json!({ "stream_id": OTHER_STREAM_ID, "events": [{ "time": OTHER_TIME, "value": "ok" }] }),
        ),
        refused,
    );
    add(
        "POST",
        "/api/status_events/batch",
        "/api/status_events/batch".into(),
        Some(json!({
            "events": [{ "site_id": OTHER_SITE_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": OTHER_TIME, "value": "ok" }],
        })),
        refused,
    );
    add(
        "POST",
        "/api/readings/import_csv",
        "/api/readings/import_csv".into(),
        Some(json!({
            "site": OTHER_SITE_ID,
            "csv": format!("time,Water Temperature\n{OTHER_TIME},5.0\n"),
            "dry_run": true,
        })),
        refused,
    );
    add(
        "POST",
        "/api/readings/sample_preview",
        "/api/readings/sample_preview".into(),
        Some(json!({ "stream_id": OTHER_STREAM_ID, "time": OTHER_TIME })),
        refused,
    );
    add(
        "POST",
        "/api/actions/backfill_calibrations",
        "/api/actions/backfill_calibrations".into(),
        Some(json!({ "sensor_ids": [OTHER_SENSOR_ID] })),
        refused,
    );
    add(
        "POST",
        "/api/actions/event_audit",
        "/api/actions/event_audit".into(),
        Some(json!({ "site_id": OTHER_SITE_ID })),
        refused,
    );
    add(
        "POST",
        "/api/actions/event_recompute",
        "/api/actions/event_recompute".into(),
        Some(json!({ "site_id": OTHER_SITE_ID })),
        refused,
    );
    add(
        "POST",
        "/api/actions/preview_derived",
        "/api/actions/preview_derived".into(),
        Some(json!({
            "formulas": [],
            "site_id": OTHER_SITE_ID,
            "start": "2024-12-31T00:00:00Z",
            "end": "2025-01-02T00:00:00Z",
        })),
        refused,
    );
    add(
        "POST",
        "/api/actions/swap",
        "/api/actions/swap".into(),
        Some(json!({
            "outgoing_sensor_id": OTHER_SENSOR_ID,
            "incoming_sensor_id": INVENTORY_SENSOR_ID,
            "site_id": OTHER_SITE_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
        })),
        refused,
    );
    add(
        "POST",
        "/api/sync/change_proposals/decide",
        "/api/sync/change_proposals/decide".into(),
        Some(json!({ "ids": [OTHER_PROPOSAL_ID], "decision": "reject" })),
        Confined::Declined("\"accepted\":0,\"rejected\":0"),
    );
    for tail in ["calculate", "preview"] {
        add(
            "POST",
            &format!("/api/tools/{{tool_name}}/{tail}"),
            format!("/api/tools/doc/{tail}"),
            Some(json!({ "site_id": OTHER_SITE_ID, "collected_at": OTHER_TIME })),
            refused,
        );
    }

    // --- Reports over every project ---
    add(
        "GET",
        "/api/change_audit",
        format!("/api/change_audit?subject=site_parameter:{OTHER_SLOT_ID}"),
        None,
        filtered,
    );
    for path in [
        "/api/actions/curation_drift",
        "/api/calculations/closure",
        "/api/calculations/health",
    ] {
        add("GET", path, path.into(), None, filtered);
    }

    // --- The project-bound CRUD entities ---
    let rows = [
        (
            "sites",
            OTHER_SITE_ID,
            json!({ "project_id": OTHER_PROJECT_ID, "name": "Not mine" }),
        ),
        (
            "site_parameters",
            OTHER_SLOT_ID,
            json!({ "site_id": OTHER_SITE_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "name": "Not mine" }),
        ),
        (
            "sensor_calibrations",
            OTHER_CALIBRATION_ID,
            json!({ "sensor_id": OTHER_SENSOR_ID, "slope": 1.0, "intercept": 0.0, "valid_from": OTHER_TIME }),
        ),
        (
            "sensor_deployments",
            OTHER_DEPLOYMENT_ID,
            json!({ "sensor_id": OTHER_SENSOR_ID, "site_id": OTHER_SITE_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "deployed_from": OTHER_TIME }),
        ),
        (
            "standard_curves",
            OTHER_CURVE_ID,
            json!({ "sensor_id": OTHER_SENSOR_ID, "slope": 1.0, "intercept": 0.0 }),
        ),
        (
            "alarm_thresholds",
            OTHER_THRESHOLD_ID,
            json!({ "site_id": OTHER_SITE_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "warning_max": 10.0 }),
        ),
        (
            "subprojects",
            OTHER_SUBPROJECT_ID,
            json!({ "project_id": OTHER_PROJECT_ID, "name": "Not mine" }),
        ),
        (
            "notes",
            OTHER_NOTE_ID,
            json!({ "site_id": OTHER_SITE_ID, "text": "not mine" }),
        ),
        (
            "annotations",
            OTHER_ANNOTATION_ID,
            json!({ "site_id": OTHER_SITE_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "start_time": OTHER_TIME, "end_time": OTHER_TIME, "text": "not mine" }),
        ),
    ];
    for (entity, id, create) in rows {
        let collection = format!("/api/{entity}");
        add("GET", &collection, collection.clone(), None, filtered);
        add(
            "POST",
            &collection,
            collection.clone(),
            Some(create),
            refused,
        );
        add(
            "DELETE",
            &format!("{collection}/{{id}}"),
            format!("{collection}/{id}"),
            None,
            refused,
        );
    }
    for entity in [
        "samples",
        "reprocessing_job_logs",
        "alarm_events",
        "tool_runs",
        "readings",
        "data_streams",
        "collection_events",
        "change_audit_entries",
        "ingest_receipts",
        "notification_mutes",
        "meteoswiss_subscriptions",
    ] {
        let collection = format!("/api/{entity}");
        add("GET", &collection, collection.clone(), None, filtered);
    }
    out
}

/// Every row a confined caller can reach either names a project, and is probed from outside it, or
/// is listed with the reason it names none. A new row cannot go unclassified.
#[test]
fn every_confinable_row_is_probed_or_named_unbound() {
    let probed: std::collections::HashSet<String> = probes().into_iter().map(|p| p.row).collect();
    let unbound: std::collections::HashSet<String> = NOT_PROJECT_BOUND
        .iter()
        .map(|(row, _)| (*row).to_string())
        .chain(global_crud_rows())
        .collect();
    let pending: std::collections::HashSet<String> = UNPROBED
        .iter()
        .chain(PROBED_ELSEWHERE.iter())
        .map(|r| (*r).to_string())
        .collect();

    let unclassified: Vec<String> = confinable_rows()
        .into_iter()
        .filter(|r| !probed.contains(r) && !unbound.contains(r) && !pending.contains(r))
        .collect();
    assert!(
        unclassified.is_empty(),
        "rows a confined caller can reach with no outside probe and no reason:\n  {}",
        unclassified.join("\n  ")
    );

    // A row the table no longer has is a misspelt or stale entry; a row a later gate change put
    // beyond every confined caller is not, so only the first is refused here.
    let rows: std::collections::HashSet<String> = table().0.iter().map(row_key).collect();
    let stray: Vec<&String> = probed
        .iter()
        .chain(pending.iter())
        .filter(|r| !rows.contains(*r))
        .collect();
    assert!(
        stray.is_empty(),
        "entries naming no row of the table: {stray:?}"
    );
    // An unprobed row a gate change put beyond every confined caller needs no probe any more.
    let confinable: std::collections::HashSet<String> = confinable_rows().into_iter().collect();
    let settled: Vec<&str> = UNPROBED
        .iter()
        .copied()
        .filter(|r| !confinable.contains(*r))
        .collect();
    assert!(
        settled.is_empty(),
        "unprobed rows no confined caller reaches, to drop from UNPROBED: {settled:?}"
    );
    let twice: Vec<&String> = probed.intersection(&pending).collect();
    assert!(twice.is_empty(), "rows both probed and unprobed: {twice:?}");
    for (row, _) in KNOWN_LEAKS {
        assert!(probed.contains(row), "a known leak with no probe: {row}");
    }
}

async fn seed_other_project(db: &sea_orm::DatabaseConnection) {
    let statements = [
        format!(
            "INSERT INTO projects (id, name, public_code) VALUES ('{OTHER_PROJECT_ID}', 'Outside', '{OTHER_CODE}')"
        ),
        format!(
            "INSERT INTO sites (id, name, project_id) VALUES ('{OTHER_SITE_ID}', 'Outside Site', '{OTHER_PROJECT_ID}')"
        ),
        format!(
            "INSERT INTO subprojects (id, project_id, name) VALUES ('{OTHER_SUBPROJECT_ID}', '{OTHER_PROJECT_ID}', 'Outside group')"
        ),
        format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name) VALUES ('{OTHER_SLOT_ID}', '{OTHER_SITE_ID}', '{GLOBAL_PARAM_TEMP_ID}', 'Outside temperature')"
        ),
        format!(
            "INSERT INTO sensors (id, name, is_active) VALUES ('{OTHER_SENSOR_ID}', 'outside', true)"
        ),
        format!(
            "INSERT INTO sensor_deployments (id, sensor_id, site_id, parameter_id, deployed_from, deployment_type) \
             VALUES ('{OTHER_DEPLOYMENT_ID}', '{OTHER_SENSOR_ID}', '{OTHER_SITE_ID}', '{GLOBAL_PARAM_TEMP_ID}', '2024-12-01T00:00:00Z', 'permanent')"
        ),
        format!(
            "INSERT INTO sensor_calibrations (id, sensor_id, parameter_id, slope, intercept, valid_from, name) \
             VALUES ('{OTHER_CALIBRATION_ID}', '{OTHER_SENSOR_ID}', '{GLOBAL_PARAM_TEMP_ID}', 2.0, 0.0, '2024-12-01T00:00:00Z', 'Outside bench')"
        ),
        format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept) VALUES ('{OTHER_CURVE_ID}', '{OTHER_SENSOR_ID}', 1.0, 0.0)"
        ),
        format!(
            "INSERT INTO data_streams (id, source_system, source_key, site_parameter_id, sensor_id, paired_at, is_active) \
             VALUES ('{OTHER_STREAM_ID}', 'test', 'outside-temperature', '{OTHER_SLOT_ID}', '{OTHER_SENSOR_ID}', now(), true)"
        ),
        format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, sensor_id) \
             VALUES ('{OTHER_STREAM_ID}', '{OTHER_SITE_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{OTHER_TIME}', 4.0, '{OTHER_SENSOR_ID}')"
        ),
        format!(
            "INSERT INTO alarm_thresholds (id, site_id, parameter_id, warning_max) VALUES ('{OTHER_THRESHOLD_ID}', '{OTHER_SITE_ID}', '{GLOBAL_PARAM_TEMP_ID}', 3.0)"
        ),
        format!(
            "INSERT INTO alarm_events (id, site_id, parameter_id, severity, max_severity, started_at, value_at_start, last_seen_at, last_value) \
             VALUES ('{OTHER_ALARM_EVENT_ID}', '{OTHER_SITE_ID}', '{GLOBAL_PARAM_TEMP_ID}', 2, 2, '{OTHER_TIME}', 4.0, '{OTHER_TIME}', 4.0)"
        ),
        format!(
            "INSERT INTO collection_events (id, site_id, collected_at) VALUES ('{OTHER_VISIT_ID}', '{OTHER_SITE_ID}', '{OTHER_TIME}')"
        ),
        format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status, site_id) VALUES ('{OTHER_JOB_ID}', 'refresh_aggregates', 'completed', '{OTHER_SITE_ID}')"
        ),
        format!(
            "INSERT INTO tool_runs (id, tool_name, tool_version, inputs, constants, curves, outputs, created_by, context) \
             VALUES ('{OTHER_TOOL_RUN_ID}', 'doc', '{{}}', '{{}}', '{{}}', '[]', '{{}}', 'test', '{{\"site_id\": \"{OTHER_SITE_ID}\", \"collected_at\": \"{OTHER_TIME}\"}}')"
        ),
        format!(
            "INSERT INTO replicate_audit_holds (id, stream_id, group_time, expected, computed, delta, site_id, parameter_id) \
             VALUES ('{OTHER_HOLD_ID}', '{OTHER_STREAM_ID}', '{OTHER_TIME}', '{{}}', '{{}}', '{{}}', '{OTHER_SITE_ID}', '{GLOBAL_PARAM_TEMP_ID}')"
        ),
        format!(
            "INSERT INTO replicate_audit_holds (id, group_time, expected, computed, delta, kind, site_id, parameter_id) \
             VALUES ('{OTHER_ENTRY_HOLD_ID}', '{OTHER_TIME}', '{{}}', '{{}}', '{{}}', 'unverified_entry', '{OTHER_SITE_ID}', '{GLOBAL_PARAM_TEMP_ID}')"
        ),
        format!(
            "INSERT INTO replicate_audit_holds (id, stream_id, group_time, expected, computed, delta, status, site_id, parameter_id) \
             VALUES ('{OTHER_DECIDED_HOLD_ID}', '{OTHER_STREAM_ID}', '2025-01-01T01:00:00Z', '{{}}', '{{}}', '{{}}', 'acknowledged', '{OTHER_SITE_ID}', '{GLOBAL_PARAM_TEMP_ID}')"
        ),
        format!(
            "INSERT INTO sensors (id, name, is_active) VALUES ('{INVENTORY_SENSOR_ID}', 'inventory', true)"
        ),
        format!(
            "INSERT INTO reading_change_proposals (id, stream_id, time, replicate_index, proposed_raw_value, stored_raw_value) \
             VALUES ('{OTHER_PROPOSAL_ID}', '{OTHER_STREAM_ID}', '{OTHER_TIME}', 0, 5.0, 4.0)"
        ),
        format!(
            "INSERT INTO notes (id, site_id, text) VALUES ('{OTHER_NOTE_ID}', '{OTHER_SITE_ID}', 'outside note')"
        ),
        format!(
            "INSERT INTO annotations (id, site_id, parameter_id, start_time, end_time, text) \
             VALUES ('{OTHER_ANNOTATION_ID}', '{OTHER_SITE_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{OTHER_TIME}', '{OTHER_TIME}', 'outside annotation')"
        ),
    ];
    for sql in &statements {
        crate::common::db::exec(db, sql).await;
    }
    for sql in other_calculation() {
        crate::common::db::exec(db, &sql).await;
    }
}

/// A calculation active only at the second project's site: it reads a parameter only that site
/// declares and publishes one no site declares, and the curation ledger holds one value it computed
/// there. Beside it, an upload session another caller opened.
fn other_calculation() -> Vec<String> {
    let manifest = json!({
        "label": "Outside calculation",
        "params": [{ "name": "x", "label": "X", "kind": "number", "required": true }],
        "event_inputs": [{ "param": "x", "parameter_code": "outside_input" }],
        "outputs": [{ "key": "o", "label": "o", "suggested_parameter_code": "outside_output" }],
    });
    vec![
        format!(
            "INSERT INTO parameters (id, code, name, category) \
             VALUES ('{OTHER_INPUT_ID}', 'outside_input', 'Outside input', 'measurement'), \
                    ('{OTHER_OUTPUT_ID}', 'outside_output', 'Outside output', 'measurement')"
        ),
        format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name) \
             VALUES ('{OTHER_INPUT_SLOT_ID}', '{OTHER_SITE_ID}', '{OTHER_INPUT_ID}', 'Outside input')"
        ),
        format!(
            "INSERT INTO tool_scripts (id, name, label, created_by) \
             VALUES ('{OTHER_CALC_ID}', 'outside_calc', 'Outside calculation', 'test')"
        ),
        format!(
            "INSERT INTO tool_script_versions (id, tool_script_id, version_no, script, entry_function, \
                 manifest, test_cases, content_hash, created_by) \
             VALUES ('{OTHER_CALC_VERSION_ID}', '{OTHER_CALC_ID}', 1, \
                 'tool <- function(inputs, constants, curves) list(o = 1)', 'tool', '{manifest}'::jsonb, \
                 '{{}}'::jsonb, md5('outside_calc'), 'test')"
        ),
        format!(
            "UPDATE tool_scripts SET active_version_id = '{OTHER_CALC_VERSION_ID}' WHERE id = '{OTHER_CALC_ID}'"
        ),
        format!(
            "INSERT INTO reading_decisions (stream_id, time, replicate_index, kind, new, actor, origin) \
             VALUES ('{OTHER_STREAM_ID}', '{OTHER_TIME}', 0, 'derived_computed', \
                 '{{\"derived_version_id\": \"{OTHER_CALC_VERSION_ID}\"}}', 'test', 'system')"
        ),
        format!(
            "INSERT INTO csv_import_chunks (session_id, seq, chunk, opened_by) \
             VALUES ('{OTHER_SESSION_ID}', 0, 'time,outside_input\n', 'someone else')"
        ),
    ]
}

/// What a probe answered, when that is not confinement, or `None` when it is.
fn leak(probe: &Probe, status: u16, body: &str) -> Option<String> {
    let refused = matches!(status, 403 | 404);
    let confined = match probe.confined {
        Confined::Refused => refused,
        Confined::Forbidden => status == 403,
        Confined::Filtered => {
            refused
                || ((200..300).contains(&status) && !OTHER_ROWS.iter().any(|id| body.contains(id)))
        }
        Confined::Declined(marker) => (200..300).contains(&status) && body.contains(marker),
        Confined::Answered(code, marker) => status == code && body.contains(marker),
    };
    (!confined).then(|| {
        let excerpt: String = body.chars().take(200).collect();
        format!(
            "{} {} answered {status}: {excerpt}",
            probe.method, probe.path
        )
    })
}

#[tokio::test]
#[serial]
async fn every_project_bound_row_is_refused_outside_the_grant() {
    if !keycloak_reachable().await {
        eprintln!("SKIP: keycloak unreachable");
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    seed_other_project(&db).await;
    let app = build_test_app_with_keycloak_admin(db.clone()).await;

    ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    grant_project(&db, &keycloak_user_id("manager1").await, PROJECT_ID).await;
    let member = get_keycloak_jwt("manager1", "manager1").await;
    let scoped =
        crate::common::seed_api_token(&db, crate::common::full_permissions(), Some(PROJECT_ID))
            .await;
    let callers = [
        (Caller::Member(Level::Manager), member),
        (Caller::ScopedToken, scoped),
    ];

    let rows = table().0;
    let known: std::collections::HashMap<&str, &str> = KNOWN_LEAKS.into_iter().collect();
    let mut leaks = Vec::new();
    for probe in probes() {
        let route = rows
            .iter()
            .find(|r| row_key(r) == probe.row)
            .unwrap_or_else(|| panic!("a probe for a row the table does not have: {}", probe.row));
        for (caller, token) in &callers {
            // A caller the gate already refuses is not confined by anything this half tests.
            if expected(route, *caller) != Outcome::Allowed {
                continue;
            }
            let (status, body) = send(&app, &probe, token).await;
            let Some(found) = leak(&probe, status, &body) else {
                continue;
            };
            let found = format!("[{}] {found}", caller.name());
            match known.get(probe.row.as_str()) {
                Some(item) => eprintln!("known leak, {item}: {found}"),
                None => leaks.push(found),
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "rows that reach a project the caller was never granted:\n  {}",
        leaks.join("\n  ")
    );

    // The unconfined half: an unscoped token reaches every one of those rows, so the refusals
    // above are confinement rather than a row nobody can reach. It runs after the confined half
    // because some probes write, and a 404 here is a row an earlier probe already moved.
    let unscoped =
        crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let mut unreachable = Vec::new();
    for probe in probes() {
        let route = rows.iter().find(|r| row_key(r) == probe.row).unwrap();
        if expected(route, Caller::FullToken) != Outcome::Allowed {
            continue;
        }
        let (status, body) = send(&app, &probe, &unscoped).await;
        if status == 401 || status == 403 {
            let excerpt: String = body.chars().take(160).collect();
            unreachable.push(format!(
                "{} {} answered {status}: {excerpt}",
                probe.method, probe.path
            ));
        }
    }
    assert!(
        unreachable.is_empty(),
        "probes an unscoped token is refused, so their confined refusal proves nothing:\n  {}",
        unreachable.join("\n  ")
    );
}

async fn send(app: &axum::Router, probe: &Probe, token: &str) -> (u16, String) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let mut req = axum::http::Request::builder()
        .method(probe.method)
        .uri(&probe.path)
        .header("Authorization", format!("Bearer {token}"));
    let req = match &probe.body {
        Some(json) => {
            req = req.header("Content-Type", "application/json");
            req.body(axum::body::Body::from(json.to_string())).unwrap()
        }
        None => req.body(axum::body::Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(req).await.expect("the router answers");
    let status = response.status().as_u16();
    let bytes = response
        .into_body()
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}
