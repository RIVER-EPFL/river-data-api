//! Permission matrix: iterate every authorization boundary against every caller type
//! and assert allow/deny. Proves the unified `/api/` tier preserves the security model.
//!
//! Caller types (in-process tests can mint API tokens; Keycloak admin path is covered by
//! the opt-in `e2e_keycloak_test.rs` suite):
//!
//!   - anonymous (no Authorization header)
//!   - read_metadata-only token
//!   - read_data-only token
//!   - write_metadata-only token
//!   - write_data-only token
//!   - full-permissions token
//!   - sync session token (full permissions, separate auth path)
//!
//! Authorization boundaries:
//!
//!   - require_read_metadata
//!   - require_read_data
//!   - require_write_metadata
//!   - require_write_data
//!   - require_crud_permissions (method-aware: GET vs mutation)
//!   - require_admin (Keycloak admin only, NO token can pass)

use serial_test::serial;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Boundary {
    ReadMeta,
    ReadData,
    WriteMeta,
    WriteData,
    Admin,
}

#[derive(Clone, Copy, Debug)]
enum Caller {
    Anonymous,
    ReadMetaOnly,
    ReadDataOnly,
    WriteMetaOnly,
    WriteDataOnly,
    Full,
    SyncSession,
}

impl Caller {
    fn name(self) -> &'static str {
        match self {
            Caller::Anonymous => "anonymous",
            Caller::ReadMetaOnly => "read_metadata-only",
            Caller::ReadDataOnly => "read_data-only",
            Caller::WriteMetaOnly => "write_metadata-only",
            Caller::WriteDataOnly => "write_data-only",
            Caller::Full => "full",
            Caller::SyncSession => "sync-session",
        }
    }
}

fn expected(boundary: Boundary, caller: Caller) -> u16 {
    // require_admin is a hard boundary: tokens always 403, anonymous 401.
    if boundary == Boundary::Admin {
        return match caller {
            Caller::Anonymous => 401,
            _ => 403,
        };
    }
    match caller {
        Caller::Anonymous => 401,
        Caller::Full | Caller::SyncSession => 200,
        Caller::ReadMetaOnly => {
            if boundary == Boundary::ReadMeta {
                200
            } else {
                403
            }
        }
        Caller::ReadDataOnly => {
            if boundary == Boundary::ReadData {
                200
            } else {
                403
            }
        }
        Caller::WriteMetaOnly => {
            if boundary == Boundary::WriteMeta {
                200
            } else {
                403
            }
        }
        Caller::WriteDataOnly => {
            if boundary == Boundary::WriteData {
                200
            } else {
                403
            }
        }
    }
}

struct Probe {
    label: &'static str,
    method: &'static str,
    path: String,
    body: Option<serde_json::Value>,
    boundary: Boundary,
    /// Allowed callers can return non-200 for non-auth reasons (e.g. 400 on validation,
    /// 404 on missing resource). We only assert auth boundaries, so accept any
    /// 2xx-or-4xx-that-isn't-auth status as "allowed".
    allow_non_200_when_authorized: bool,
}

async fn issue(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
    token: Option<&str>,
) -> u16 {
    match (method, body) {
        ("GET", _) => match token {
            Some(t) => crate::common::get_with_token(app, path, t).await.0,
            None => crate::common::get(app, path).await.0,
        },
        ("POST", Some(b)) => match token {
            Some(t) => crate::common::post_json_with_token(app, path, b, t).await.0,
            None => {
                let empty = "Bearer ";
                crate::common::get_with_auth_header(app, path, empty)
                    .await
                    .0
            }
        },
        ("POST", None) => match token {
            Some(t) => {
                crate::common::post_json_with_token(app, path, &serde_json::json!({}), t)
                    .await
                    .0
            }
            None => {
                let empty = "Bearer ";
                crate::common::get_with_auth_header(app, path, empty)
                    .await
                    .0
            }
        },
        ("PATCH", Some(b)) => match token {
            Some(t) => {
                crate::common::patch_json_with_token(app, path, b, t)
                    .await
                    .0
            }
            None => 401,
        },
        ("DELETE", _) => match token {
            Some(t) => crate::common::delete_with_token(app, path, t).await.0,
            None => 401,
        },
        _ => panic!("unsupported method/body combo"),
    }
}

fn check_status(probe: &Probe, caller: Caller, actual: u16) {
    let want = expected(probe.boundary, caller);
    if want == 200 && probe.allow_non_200_when_authorized {
        // Authorized, any non-auth status is fine (handler may 400, 404, 422 for reasons unrelated to auth).
        assert!(
            !(401..=403).contains(&actual),
            "[{}] {} as {} expected ALLOWED (non-auth status), got {}",
            probe.label,
            probe.path,
            caller.name(),
            actual
        );
    } else {
        assert_eq!(
            actual,
            want,
            "[{}] {} as {} expected {} got {}",
            probe.label,
            probe.path,
            caller.name(),
            want,
            actual
        );
    }
}

#[tokio::test]
#[serial]
async fn permission_matrix_covers_every_boundary() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let site_id = crate::common::fixtures::SITE1_ID;

    let tok_full = crate::common::seed_token_full(&db).await;
    let tok_read_meta = crate::common::seed_token_read_metadata_only(&db).await;
    let tok_read_data = crate::common::seed_token_read_data_only(&db).await;
    let tok_write_meta = crate::common::seed_token_write_metadata_only(&db).await;
    let tok_write_data = crate::common::seed_token_write_data_only(&db).await;
    let (tok_sync, _service_id) = crate::common::seed_sync_session_token(&db).await;

    let callers: Vec<(Caller, Option<&str>)> = vec![
        (Caller::Anonymous, None),
        (Caller::ReadMetaOnly, Some(&tok_read_meta)),
        (Caller::ReadDataOnly, Some(&tok_read_data)),
        (Caller::WriteMetaOnly, Some(&tok_write_meta)),
        (Caller::WriteDataOnly, Some(&tok_write_data)),
        (Caller::Full, Some(&tok_full)),
        (Caller::SyncSession, Some(&tok_sync)),
    ];

    let now = chrono::Utc::now();
    let start = (now - chrono::Duration::days(2)).to_rfc3339();
    let end = now.to_rfc3339();

    let probes: Vec<Probe> = vec![
        // require_read_metadata
        Probe {
            label: "search",
            method: "GET",
            path: "/api/search?q=Site".to_string(),
            body: None,
            boundary: Boundary::ReadMeta,
            allow_non_200_when_authorized: false,
        },
        // require_read_data
        Probe {
            label: "alarms-summary",
            method: "GET",
            path: "/api/alarms/summary".to_string(),
            body: None,
            boundary: Boundary::ReadData,
            allow_non_200_when_authorized: false,
        },
        Probe {
            label: "tools-list",
            method: "GET",
            path: "/api/tools".to_string(),
            body: None,
            boundary: Boundary::ReadData,
            allow_non_200_when_authorized: false,
        },
        Probe {
            label: "tools-calculate",
            method: "POST",
            path: "/api/tools/field_data/calculate".to_string(),
            body: Some(serde_json::json!({ "inputs": {} })),
            boundary: Boundary::ReadData,
            allow_non_200_when_authorized: true,
        },
        Probe {
            label: "site-readings",
            method: "GET",
            path: format!("/api/sites/{site_id}/readings?start={start}&end={end}"),
            body: None,
            boundary: Boundary::ReadData,
            allow_non_200_when_authorized: true,
        },
        // require_write_metadata (operator action)
        Probe {
            label: "merge-site-parameters",
            method: "POST",
            path: "/api/actions/merge_site_parameters".to_string(),
            body: Some(serde_json::json!({})),
            boundary: Boundary::WriteMeta,
            allow_non_200_when_authorized: true,
        },
        Probe {
            label: "register-stream",
            method: "POST",
            path: "/api/streams/register".to_string(),
            body: Some(serde_json::json!({})),
            boundary: Boundary::WriteMeta,
            allow_non_200_when_authorized: true,
        },
        // require_write_data
        Probe {
            label: "refresh-aggregates",
            method: "POST",
            path: "/api/actions/refresh_aggregates".to_string(),
            body: Some(serde_json::json!({})),
            boundary: Boundary::WriteData,
            allow_non_200_when_authorized: true,
        },
        Probe {
            label: "readings-batch",
            method: "POST",
            path: "/api/readings/batch".to_string(),
            body: Some(serde_json::json!([])),
            boundary: Boundary::WriteData,
            allow_non_200_when_authorized: true,
        },
        Probe {
            label: "grab-samples",
            method: "POST",
            path: "/api/grab_samples".to_string(),
            body: Some(serde_json::json!({})),
            boundary: Boundary::WriteData,
            allow_non_200_when_authorized: true,
        },
        Probe {
            label: "status-events-batch",
            method: "POST",
            path: "/api/status_events/batch".to_string(),
            body: Some(serde_json::json!({})),
            boundary: Boundary::WriteData,
            allow_non_200_when_authorized: true,
        },
        Probe {
            label: "ingest",
            method: "POST",
            path: "/api/ingest".to_string(),
            body: Some(serde_json::json!({})),
            boundary: Boundary::WriteData,
            allow_non_200_when_authorized: true,
        },
        Probe {
            label: "import-csv",
            method: "POST",
            path: "/api/readings/import_csv".to_string(),
            body: Some(serde_json::json!({})),
            boundary: Boundary::WriteData,
            allow_non_200_when_authorized: true,
        },
        Probe {
            label: "readings-flag",
            method: "PATCH",
            path: "/api/readings/flag".to_string(),
            body: Some(serde_json::json!({})),
            boundary: Boundary::WriteData,
            allow_non_200_when_authorized: true,
        },
        // require_admin, no token can pass
        Probe {
            label: "tokens-create",
            method: "POST",
            path: "/api/tokens".to_string(),
            body: Some(serde_json::json!({
                "name": "should-be-denied",
                "permissions": {"read_metadata": true, "read_data": false, "write_metadata": false, "write_data": false}
            })),
            boundary: Boundary::Admin,
            allow_non_200_when_authorized: true,
        },
        Probe {
            label: "tokens-list",
            method: "GET",
            path: "/api/tokens".to_string(),
            body: None,
            boundary: Boundary::Admin,
            allow_non_200_when_authorized: true,
        },
        Probe {
            label: "sync-credentials-list",
            method: "GET",
            path: "/api/sync_service_credentials".to_string(),
            body: None,
            boundary: Boundary::Admin,
            allow_non_200_when_authorized: true,
        },
        Probe {
            label: "api-token-audit-list",
            method: "GET",
            path: "/api/api_token_audit_logs".to_string(),
            body: None,
            boundary: Boundary::Admin,
            allow_non_200_when_authorized: true,
        },
        Probe {
            label: "sync-credentials-create-via-core",
            method: "POST",
            path: "/api/sync/credentials".to_string(),
            body: Some(serde_json::json!({"name": "test"})),
            boundary: Boundary::Admin,
            allow_non_200_when_authorized: true,
        },
    ];

    for probe in &probes {
        for (caller, token) in &callers {
            let actual = issue(&app, probe.method, &probe.path, probe.body.as_ref(), *token).await;
            check_status(probe, *caller, actual);
        }
    }
}

#[tokio::test]
#[serial]
async fn require_admin_blocks_full_permissions_token() {
    // Spotlight on the most important security property: even a token with EVERY
    // permission scope set to true cannot reach require_admin routes. Catches the
    // common regression where a future scope addition (e.g. add `admin: bool` to
    // TokenPermissions) silently weakens the boundary.
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let tok_full = crate::common::seed_token_full(&db).await;

    let admin_paths = [
        ("GET", "/api/tokens"),
        ("GET", "/api/sync_service_credentials"),
        ("POST", "/api/sync/credentials"),
    ];

    for (method, path) in admin_paths {
        let status = match method {
            "GET" => crate::common::get_with_token(&app, path, &tok_full).await.0,
            "POST" => {
                crate::common::post_json_with_token(&app, path, &serde_json::json!({}), &tok_full)
                    .await
                    .0
            }
            _ => unreachable!(),
        };
        assert_eq!(
            status, 403,
            "{method} {path} with full-permissions token must be 403"
        );
    }
}

#[tokio::test]
#[serial]
async fn sync_session_token_full_permissions_but_not_admin() {
    // Sync session tokens get FULL TokenPermissions in middleware.rs:159-165.
    // They MUST still be blocked by require_admin (escalation safety: a stolen
    // sync token cannot reach user management or credential mutation).
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (tok_sync, _) = crate::common::seed_sync_session_token(&db).await;

    let (status, _) = crate::common::get_with_token(&app, "/api/search?q=site", &tok_sync).await;
    assert_eq!(status, 200, "sync session can hit read_metadata routes");

    let (status, _) = crate::common::get_with_token(&app, "/api/tokens", &tok_sync).await;
    assert_eq!(status, 403, "sync session must NOT pass require_admin");

    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/sync/credentials",
        &serde_json::json!({"name": "should-be-denied"}),
        &tok_sync,
    )
    .await;
    assert_eq!(status, 403, "sync session cannot mint new credentials");
}

/// A standard curve is field metadata: reading one needs the metadata read bit, minting or removing
/// one needs the metadata write bit. Stated as its own test because the curve is the provenance of
/// published values, so which bit reaches it is a security boundary in its own right.
#[tokio::test]
#[serial]
async fn a_token_needs_write_metadata_to_mint_a_curve_and_read_metadata_to_see_one() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor_id = uuid::Uuid::new_v4();
    crate::common::db::exec(
        &db,
        &format!("INSERT INTO sensors (id, name) VALUES ('{sensor_id}', 'Plate reader')"),
    )
    .await;

    let tok_read_meta = crate::common::seed_token_read_metadata_only(&db).await;
    let tok_read_data = crate::common::seed_token_read_data_only(&db).await;
    let tok_write_meta = crate::common::seed_token_write_metadata_only(&db).await;
    let tok_write_data = crate::common::seed_token_write_data_only(&db).await;

    let curve = serde_json::json!({
        "sensor_id": sensor_id, "name": "Plate A", "slope": 3.0, "intercept": 0.5,
    });

    let (s, _) = crate::common::get_with_token(&app, "/api/standard_curves", &tok_read_meta).await;
    assert_eq!(s, 200, "the metadata read bit lists curves");
    let (s, _) = crate::common::get_with_token(&app, "/api/standard_curves", &tok_read_data).await;
    assert_eq!(s, 403, "the data read bit does not");

    for (label, token) in [
        ("read_metadata-only", &tok_read_meta),
        ("read_data-only", &tok_read_data),
        ("write_data-only", &tok_write_data),
    ] {
        let (s, _) =
            crate::common::post_json_with_token(&app, "/api/standard_curves", &curve, token).await;
        assert_eq!(s, 403, "a {label} token cannot mint a curve");
    }

    let (s, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/standard_curves",
        &curve,
        &tok_write_meta,
    )
    .await;
    assert!(
        (200..300).contains(&s),
        "the metadata write bit mints a curve: {body}"
    );
    let curve_id = body["id"]
        .as_str()
        .expect("the created curve carries an id");

    for (label, token) in [
        ("read_metadata-only", &tok_read_meta),
        ("write_data-only", &tok_write_data),
    ] {
        let (s, _) = crate::common::delete_with_token(
            &app,
            &format!("/api/standard_curves/{curve_id}"),
            token,
        )
        .await;
        assert_eq!(s, 403, "a {label} token cannot remove a curve");
    }

    // CrudCrate exposes update as PUT, and a body carrying only the fields to change.
    let (s, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/standard_curves/{curve_id}"),
        &serde_json::json!({ "notes": "re-fitted from the plate log" }),
        &tok_write_meta,
    )
    .await;
    assert!(
        (200..300).contains(&s),
        "the metadata write bit edits a curve: {s} {body}"
    );

    let (s, _) = crate::common::delete_with_token(
        &app,
        &format!("/api/standard_curves/{curve_id}"),
        &tok_write_meta,
    )
    .await;
    assert!(
        (200..300).contains(&s),
        "and removes one no reading has used"
    );
}
