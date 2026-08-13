//! The API-key administration surface driven through the router, as the dashboard drives it.
//!
//! Scenario: an administrator mints an external key from the Tokens screen, the key is used, then
//! rotated and revoked; afterwards the forensic views are consulted.
//! Expected behaviour: every one of those steps is reachable only for a Keycloak Administrator,
//! and each one takes effect on the very next request.
//!
//! The sibling suite `token_lifecycle.rs` proves the same guarantees by calling `revoke_token` and
//! `rotate_token` as functions, which never touches the route table or the `require_admin` layer.
//! Everything here goes over HTTP, so the wiring is part of what is asserted. The platform probes
//! (`/readyz`, `/api/version`, `/api/config/keycloak`) sit here because they share the same
//! question: what is reachable, by whom, without a person's JWT.

use serial_test::serial;

use crate::common::fixtures::{
    GLOBAL_PARAM_DEPTH_ID, GLOBAL_PARAM_TEMP_ID, PROJECT_ID, SITE1_ID, SITE2_ID,
};
use crate::common::keycloak as kc;

/// The four realm roles the role picker is allowed to offer (`src/common/authz.rs`
/// `RIVER_ROLE_NAMES`).
const RIVER_ROLE_NAMES: [&str; 4] = [
    "riverdata-admin",
    "riverdata-manager",
    "riverdata-river",
    "riverdata-intern",
];

/// Ids that resolve to nothing, for the not-found branches.
const UNKNOWN_TOKEN_ID: &str = "00000000-0000-4000-e000-0000000000ff";
const UNKNOWN_SENSOR_ID: &str = "00000000-0000-4000-c000-0000000000ff";

fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The non-secret lookup prefix of `rvd_<prefix>_<secret>`.
fn lookup_prefix(raw: &str) -> String {
    raw.strip_prefix("rvd_")
        .and_then(|r| r.split_once('_'))
        .map(|(p, _)| p.to_string())
        .unwrap_or_else(|| panic!("api token must be rvd_<prefix>_<secret>, got {raw}"))
}

/// The secret half of `rvd_<prefix>_<secret>`.
fn secret_half(raw: &str) -> String {
    raw.strip_prefix("rvd_")
        .and_then(|r| r.split_once('_'))
        .map(|(_, s)| s.to_string())
        .unwrap_or_else(|| panic!("api token must be rvd_<prefix>_<secret>, got {raw}"))
}

async fn seeded_db() -> sea_orm::DatabaseConnection {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    db
}

#[tokio::test]
#[serial]
async fn create_use_rotate_and_revoke_a_key_over_http() {
    if !kc::require_keycloak_or_skip("create_use_rotate_and_revoke_a_key_over_http").await {
        return;
    }
    let db = seeded_db().await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    kc::ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    let manager = kc::get_keycloak_jwt("manager1", "manager1").await;

    // The payload the Tokens screen posts (river-data-ui/src/routes/tokens/new/+page.svelte).
    let payload = serde_json::json!({
        "name": "logger-key",
        "description": "external logger",
        "permissions": {
            "read_metadata": true,
            "read_data": true,
            "write_metadata": false,
            "write_data": true
        },
        "project_scope": PROJECT_ID,
        "created_by": "admin",
    });

    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/tokens", &payload, &manager).await;
    assert_eq!(
        status, 403,
        "minting a key is Administrator-only, a manager is refused: {body}"
    );

    let (status, created) =
        crate::common::post_json_parse_with_token(&app, "/api/tokens", &payload, &admin).await;
    assert_eq!(
        status, 201,
        "an administrator mints a key over HTTP: {created}"
    );

    let token_id = created["id"]
        .as_str()
        .expect("created token id")
        .to_string();
    token_id
        .parse::<uuid::Uuid>()
        .unwrap_or_else(|e| panic!("token id is a uuid: {e}"));
    let secret = created["token"]
        .as_str()
        .unwrap_or_else(|| panic!("the raw secret is returned exactly once, on create: {created}"))
        .to_string();
    assert!(
        secret.starts_with("rvd_"),
        "minted secret carries the rvd_ prefix: {secret}"
    );
    assert!(
        !lookup_prefix(&secret).is_empty(),
        "minted secret has a lookup prefix: {secret}"
    );
    assert!(
        !secret_half(&secret).is_empty(),
        "minted secret has a secret half: {secret}"
    );
    assert_eq!(
        created["is_active"], true,
        "a fresh key is active: {created}"
    );
    assert_eq!(
        created["name"], "logger-key",
        "create echoes the name: {created}"
    );
    assert_eq!(
        created["project_scope"], PROJECT_ID,
        "create stores the project scope: {created}"
    );
    assert_eq!(
        created["permissions"]["write_data"], true,
        "the permissions object round-trips: {created}"
    );
    assert_eq!(
        created["permissions"]["write_metadata"], false,
        "the permissions object round-trips: {created}"
    );

    let (status, body) = crate::common::get_with_token(&app, "/api/sites", &secret).await;
    assert_eq!(
        status, 200,
        "the secret the create response returned authenticates: {body}"
    );

    let batch = serde_json::json!({
        "readings": [{
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "time": now_rfc3339(),
            "raw_value": 12.5
        }]
    });
    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/readings/batch", &batch, &secret).await;
    assert_eq!(
        status, 200,
        "the write_data bit sent on create is granted: {body}"
    );

    // Scope is checked ahead of capability, and this slot is inside the key's own project (the
    // identical create succeeds for a scoped key that carries write_metadata,
    // tests/auth/token_lifecycle.rs::scoped_metadata_key_confined_and_blocked_from_global_catalog),
    // so the refusal here is the withheld write_metadata bit.
    let site_parameter =
        serde_json::json!({ "site_id": SITE2_ID, "parameter_id": GLOBAL_PARAM_DEPTH_ID });
    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/site_parameters", &site_parameter, &secret)
            .await;
    assert_eq!(
        status, 403,
        "the withheld write_metadata bit is enforced: {body}"
    );

    let (status, list) = crate::common::get_json_with_token(&app, "/api/tokens", &admin).await;
    assert_eq!(status, 200, "an administrator lists the keys: {list}");
    let listed = list
        .as_array()
        .unwrap_or_else(|| panic!("the token list is an array: {list}"))
        .iter()
        .find(|t| t["id"].as_str() == Some(token_id.as_str()))
        .cloned();
    assert!(
        listed.is_some(),
        "the minted key appears in the list: {list}"
    );
    let listed = listed.expect("listed token");
    // The list model carries the `token` key with a null value rather than omitting it (the
    // model's `skip_serializing_if` does not survive CrudCrate's list-model codegen); either
    // shape satisfies the property, which is that no secret comes back.
    assert!(
        listed.get("token").is_none_or(serde_json::Value::is_null),
        "the list never re-emits a secret: {listed}"
    );
    assert!(
        listed.get("token_hash").is_none(),
        "the list never emits the hash: {listed}"
    );
    assert_eq!(
        listed["token_prefix"],
        lookup_prefix(&secret),
        "the list carries the non-secret lookup prefix: {listed}"
    );

    let rotate_path = format!("/api/tokens/{token_id}/rotate");
    let revoke_path = format!("/api/tokens/{token_id}/revoke");
    let no_body = serde_json::json!({});

    let (status, body) =
        crate::common::post_json_with_token(&app, &rotate_path, &no_body, &manager).await;
    assert_eq!(
        status, 403,
        "rotation is Administrator-only, a manager is refused: {body}"
    );
    let (status, body) =
        crate::common::post_json_with_token(&app, &revoke_path, &no_body, &manager).await;
    assert_eq!(
        status, 403,
        "revocation is Administrator-only, a manager is refused: {body}"
    );
    let (status, body) = crate::common::get_with_token(&app, "/api/sites", &secret).await;
    assert_eq!(
        status, 200,
        "a refused rotate/revoke changed nothing about the key: {body}"
    );

    let (status, rotated) =
        crate::common::post_json_parse_with_token(&app, &rotate_path, &no_body, &admin).await;
    assert_eq!(status, 200, "an administrator rotates the key: {rotated}");
    let new_secret = rotated["token"]
        .as_str()
        .unwrap_or_else(|| panic!("rotation returns the new secret once: {rotated}"))
        .to_string();
    assert!(
        new_secret.starts_with("rvd_"),
        "rotated secret carries the prefix: {new_secret}"
    );
    assert_ne!(new_secret, secret, "rotation mints a different secret");
    assert_eq!(
        rotated["id"].as_str(),
        Some(token_id.as_str()),
        "rotation keeps the id: {rotated}"
    );
    assert_eq!(
        rotated["name"], "logger-key",
        "rotation preserves the name: {rotated}"
    );
    assert_eq!(
        rotated["project_scope"], PROJECT_ID,
        "rotation preserves the project scope: {rotated}"
    );
    assert_eq!(
        rotated["is_active"], true,
        "rotating an active key leaves it active: {rotated}"
    );

    let (status, body) = crate::common::get_with_token(&app, "/api/sites", &secret).await;
    assert_eq!(
        status, 401,
        "the previous secret stops working immediately: {body}"
    );
    let (status, body) = crate::common::get_with_token(&app, "/api/sites", &new_secret).await;
    assert_eq!(status, 200, "the rotated secret authenticates: {body}");

    let (status, revoked) =
        crate::common::post_json_parse_with_token(&app, &revoke_path, &no_body, &admin).await;
    assert_eq!(status, 200, "an administrator revokes the key: {revoked}");
    assert_eq!(
        revoked["is_active"], false,
        "revocation deactivates the key: {revoked}"
    );
    assert!(
        revoked.get("token").is_none_or(serde_json::Value::is_null),
        "revocation neither mints nor echoes a secret: {revoked}"
    );

    let (status, body) = crate::common::get_with_token(&app, "/api/sites", &new_secret).await;
    assert_eq!(
        status, 401,
        "a revoked key fails on the very next request: {body}"
    );

    let (status, fetched) =
        crate::common::get_json_with_token(&app, &format!("/api/tokens/{token_id}"), &admin).await;
    assert_eq!(
        status, 200,
        "the revoked key is still readable by an administrator: {fetched}"
    );
    assert_eq!(
        fetched["is_active"], false,
        "revocation is persisted: {fetched}"
    );
    assert_eq!(
        fetched["token_prefix"],
        lookup_prefix(&new_secret),
        "the rotated lookup prefix is the persisted one: {fetched}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tokens/{UNKNOWN_TOKEN_ID}/revoke"),
        &no_body,
        &admin,
    )
    .await;
    assert_eq!(
        status, 404,
        "revoke resolves the id rather than ignoring it: {body}"
    );
    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tokens/{UNKNOWN_TOKEN_ID}/rotate"),
        &no_body,
        &admin,
    )
    .await;
    assert_eq!(
        status, 404,
        "rotate resolves the id rather than ignoring it: {body}"
    );
}

#[tokio::test]
#[serial]
async fn usage_view_and_audit_status_codes_over_http() {
    if !kc::require_keycloak_or_skip("usage_view_and_audit_status_codes_over_http").await {
        return;
    }
    let db = seeded_db().await;
    // Two apps over one database: the audit app is the only one configured to record token use,
    // the Keycloak app is the only one a person's JWT can clear `require_admin` on. The forensic
    // views read the rows the audit app wrote.
    let kc_app = kc::build_test_app_with_keycloak(db.clone()).await;
    let audit_app = crate::common::build_test_app_with_audit(db.clone()).0;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    kc::ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    let manager = kc::get_keycloak_jwt("manager1", "manager1").await;

    let read_only = serde_json::json!({
        "read_metadata": true,
        "read_data": true,
        "write_metadata": false,
        "write_data": false
    });

    let (status, used) = crate::common::post_json_parse_with_token(
        &kc_app,
        "/api/tokens",
        &serde_json::json!({ "name": "audited-key", "permissions": read_only }),
        &admin,
    )
    .await;
    assert_eq!(status, 201, "mint the key whose use is audited: {used}");
    let used_id = used["id"].as_str().expect("used token id").to_string();
    let used_secret = used["token"]
        .as_str()
        .expect("used token secret")
        .to_string();

    let (status, idle) = crate::common::post_json_parse_with_token(
        &kc_app,
        "/api/tokens",
        &serde_json::json!({ "name": "idle-key", "permissions": read_only }),
        &admin,
    )
    .await;
    assert_eq!(status, 201, "mint a key that is never used: {idle}");
    let idle_id = idle["id"].as_str().expect("idle token id").to_string();

    // The only two requests this key ever makes, one hit and one miss, so the usage view's
    // contents are exactly known. Every later call in this test uses the admin JWT, which is not
    // audited (auditing records API-token use only).
    let (status, body) =
        crate::common::get_with_token(&audit_app, "/api/sites", &used_secret).await;
    assert_eq!(status, 200, "the audited key reads sites: {body}");
    let missing_sensor = format!("/api/sensors/{UNKNOWN_SENSOR_ID}/readings");
    let (status, body) =
        crate::common::get_with_token(&audit_app, &missing_sensor, &used_secret).await;
    assert_eq!(status, 404, "an unknown sensor is a recorded 404: {body}");

    let usage_path = format!("/api/tokens/{used_id}/usage");
    let mut usage = serde_json::Value::Null;
    for _ in 0..40 {
        let (status, body) = crate::common::get_json_with_token(&kc_app, &usage_path, &admin).await;
        assert_eq!(
            status, 200,
            "the usage view answers an administrator: {body}"
        );
        let settled = body.as_array().is_some_and(|entries| entries.len() >= 2);
        usage = body;
        if settled {
            break;
        }
        // The audit write is fire-and-forget, so poll for it rather than assuming it has landed.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let entries = usage
        .as_array()
        .unwrap_or_else(|| panic!("the usage view answers with an array, got {usage}"));
    assert_eq!(
        entries.len(),
        2,
        "the key's two requests are recorded, and nothing else is attributed to it: {usage}"
    );

    let sites_entries: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| e["method"] == "GET" && e["path"] == "/sites")
        .collect();
    assert_eq!(
        sites_entries.len(),
        1,
        "the sites read is recorded once, under the nest-stripped path: {usage}"
    );
    assert_eq!(
        sites_entries[0]["status_code"].as_i64(),
        Some(200),
        "the recorded outcome is the served status: {usage}"
    );

    let sensor_entries: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| {
            e["path"]
                .as_str()
                .is_some_and(|p| p.starts_with("/sensors/"))
        })
        .collect();
    assert_eq!(
        sensor_entries.len(),
        1,
        "the failed sensor read is recorded once: {usage}"
    );
    assert_eq!(
        sensor_entries[0]["status_code"].as_i64(),
        Some(404),
        "a non-200 outcome is recorded as itself, not normalised: {usage}"
    );

    for entry in entries {
        assert!(
            entry["project_scope"].is_null(),
            "an unscoped key records no project scope: {entry}"
        );
    }

    let (status, idle_usage) = crate::common::get_json_with_token(
        &kc_app,
        &format!("/api/tokens/{idle_id}/usage"),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "the usage view answers for an unused key: {idle_usage}"
    );
    assert_eq!(
        idle_usage.as_array().map(Vec::len),
        Some(0),
        "usage is filtered to the requested key, not a global dump: {idle_usage}"
    );

    let (status, body) = crate::common::get_with_token(&kc_app, &usage_path, &manager).await;
    assert_eq!(status, 403, "the usage view is Administrator-only: {body}");
    let (status, body) = crate::common::get(&kc_app, &usage_path).await;
    assert_eq!(
        status, 401,
        "the usage view refuses an anonymous caller: {body}"
    );

    let codes_path = "/api/api_token_audit_logs/distinct/status_codes";
    let (status, codes_body) =
        crate::common::get_json_with_token(&kc_app, codes_path, &admin).await;
    assert_eq!(
        status, 200,
        "the audit filter's option list answers: {codes_body}"
    );
    let codes: Vec<i64> = codes_body["status_codes"]
        .as_array()
        .unwrap_or_else(|| panic!("status_codes is an array: {codes_body}"))
        .iter()
        .map(|c| {
            c.as_i64()
                .unwrap_or_else(|| panic!("status code is an integer: {codes_body}"))
        })
        .collect();
    // `api_token_audit_log` is never truncated between tests (tests/common/db.rs), so this is a
    // global aggregate and only containment of the two codes this test produced can be asserted.
    assert!(
        codes.contains(&200),
        "the 200 this test recorded is offered as a filter: {codes:?}"
    );
    assert!(
        codes.contains(&404),
        "the 404 this test recorded is offered as a filter: {codes:?}"
    );
    assert!(
        codes.windows(2).all(|w| w[0] < w[1]),
        "the documented contract is distinct codes, ascending: {codes:?}"
    );

    let (status, body) = crate::common::get_with_token(&kc_app, codes_path, &manager).await;
    assert_eq!(
        status, 403,
        "the audit filter list is Administrator-only: {body}"
    );
}

#[tokio::test]
#[serial]
async fn roles_endpoint_lists_only_river_access_levels() {
    if !kc::require_keycloak_or_skip("roles_endpoint_lists_only_river_access_levels").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak_admin(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    kc::ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    let manager = kc::get_keycloak_jwt("manager1", "manager1").await;

    let (status, body) = crate::common::get_json_with_token(&app, "/api/roles", &admin).await;
    assert_eq!(
        status, 200,
        "an administrator reads the role picker's options: {body}"
    );
    let roles = body
        .as_array()
        .unwrap_or_else(|| panic!("the roles response is an array: {body}"));

    let mut names = Vec::new();
    for role in roles {
        let object = role
            .as_object()
            .unwrap_or_else(|| panic!("each role is an object: {body}"));
        assert_eq!(object.len(), 2, "a role carries only id and name: {role}");
        assert!(object.contains_key("id"), "a role carries an id: {role}");
        assert!(object.contains_key("name"), "a role carries a name: {role}");
        assert!(
            !object["id"].as_str().unwrap_or_default().is_empty(),
            "the picker needs a usable role id: {role}"
        );
        names.push(object["name"].as_str().unwrap_or_default().to_string());
    }

    for name in &names {
        assert!(
            RIVER_ROLE_NAMES.contains(&name.as_str()),
            "the picker must offer no Keycloak internals or bare admin role, got {names:?}"
        );
    }
    // Both of these are held by the two users this test authenticates as, so both exist in the
    // realm and an empty or over-filtered response fails here.
    assert!(
        names.contains(&"riverdata-admin".to_string()),
        "roles: {names:?}"
    );
    assert!(
        names.contains(&"riverdata-manager".to_string()),
        "roles: {names:?}"
    );

    let (status, body) = crate::common::get_with_token(&app, "/api/roles", &manager).await;
    assert_eq!(status, 403, "the role list is Administrator-only: {body}");
    let (status, body) = crate::common::get(&app, "/api/roles").await;
    assert_eq!(
        status, 401,
        "an anonymous caller is unauthenticated, not merely forbidden: {body}"
    );
}

#[tokio::test]
#[serial]
async fn readiness_probe_version_and_keycloak_config_surface() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    // `test_config` leaves keycloak_url/realm/client_id unset, which is the unconfigured case the
    // frontend bootstrap endpoint has to answer for.
    let app = crate::common::build_test_app(db.clone());

    let (status, body) = crate::common::get(&app, "/readyz").await;
    assert_eq!(
        status, 200,
        "the readiness probe reports a reachable database: {body}"
    );

    let (status, body) = crate::common::get(&app, "/api/version").await;
    assert_eq!(
        status, 401,
        "build metadata is not served anonymously: {body}"
    );

    let read_metadata = crate::common::seed_token_read_metadata_only(&db).await;
    let (status, version) =
        crate::common::get_json_with_token(&app, "/api/version", &read_metadata).await;
    assert_eq!(
        status, 200,
        "read_metadata reaches the version endpoint: {version}"
    );
    assert_eq!(
        version["name"], "river-db",
        "the reported name is the crate's: {version}"
    );
    assert_eq!(
        version["version"],
        env!("CARGO_PKG_VERSION"),
        "the reported version is the compiled-in crate version: {version}"
    );
    // Both are baked in at compile time, falling back to these literals when CI passes no build
    // args; the test resolves them the same way so it holds for a tagged build too.
    assert_eq!(
        version["commit"],
        option_env!("BUILD_VERSION").unwrap_or("dev"),
        "the reported commit is the compiled-in build arg: {version}"
    );
    assert_eq!(
        version["built_at"],
        option_env!("BUILD_TIME").unwrap_or("unknown"),
        "the reported build time is the compiled-in build arg: {version}"
    );

    let read_data = crate::common::seed_token_read_data_only(&db).await;
    let (status, body) = crate::common::get_with_token(&app, "/api/version", &read_data).await;
    assert_eq!(
        status, 403,
        "a key without read_metadata is refused the version: {body}"
    );

    let (status, body) = crate::common::get(&app, "/api/config/keycloak").await;
    assert_eq!(
        status, 404,
        "an unconfigured Keycloak must 404 rather than answer with a partial config: {body}"
    );
}
