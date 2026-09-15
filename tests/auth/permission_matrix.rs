//! One table for the whole authorization surface: every registered route against every kind of
//! caller.
//!
//! Three things are separated deliberately.
//!
//! - **The policy** (which access level holds which capability, which token bit satisfies it) is a
//!   pure function tested inline in `src/common/authz.rs`, with no database.
//! - **The wiring** (which capability each route sits behind) is the table below, one row per
//!   registered route. `every_registered_route_has_a_row` reads `routes/service/mod.rs` and the
//!   routers it nests, so a route added without a row fails rather than going unprobed.
//! - **The enforcement** is one loop issuing every (route, caller) pair and asserting the outcome
//!   `river_db::common::authz` says to expect. Nothing here restates the policy; a gate wired to
//!   the wrong capability is what it catches.
//!
//! Callers: anonymous, the five API-token bit patterns, a sync session token, a project-scoped
//! token, and the four Keycloak levels (Intern < River < Manager < Administrator). The four levels
//! and the scoped token need the dev Keycloak; the suite skips when it is unreachable.

use river_db::common::authz::{
    Capability, Role, TokenAccess, TokenBit, TokenPermissions, keycloak_allows, token_allows,
};
use serial_test::serial;

use crate::common::fixtures::{PROJECT_ID, SITE1_ID};
use crate::common::keycloak::{
    build_test_app_with_keycloak_admin, ensure_realm_user, get_keycloak_jwt, grant_project,
    keycloak_reachable, keycloak_user_id,
};

/// A project the fixture callers are never granted, for the denial half of every project-bound row.
const OTHER_PROJECT_ID: &str = "00000000-0000-4000-a000-000000000099";
const OTHER_SITE_ID: &str = "00000000-0000-4000-a000-000000000098";

// --- The callers ---

#[derive(Clone, Copy, Debug, PartialEq)]
enum Caller {
    Anonymous,
    ReadMetaToken,
    ReadDataToken,
    WriteMetaToken,
    WriteDataToken,
    FullToken,
    SyncSession,
    ScopedToken,
    Member(Level),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Level {
    Intern,
    River,
    Manager,
    Administrator,
}

impl Level {
    fn role(self) -> Role {
        match self {
            Level::Intern => Role::Intern,
            Level::River => Role::River,
            Level::Manager => Role::Manager,
            Level::Administrator => Role::Administrator,
        }
    }

    fn username(self) -> &'static str {
        match self {
            Level::Intern => "intern1",
            Level::River => "river1",
            Level::Manager => "manager1",
            Level::Administrator => "admin",
        }
    }
}

impl Caller {
    fn name(self) -> String {
        match self {
            Caller::Anonymous => "anonymous".to_string(),
            Caller::ReadMetaToken => "read_metadata-only token".to_string(),
            Caller::ReadDataToken => "read_data-only token".to_string(),
            Caller::WriteMetaToken => "write_metadata-only token".to_string(),
            Caller::WriteDataToken => "write_data-only token".to_string(),
            Caller::FullToken => "full token".to_string(),
            Caller::SyncSession => "sync session token".to_string(),
            Caller::ScopedToken => "project-scoped token".to_string(),
            Caller::Member(level) => format!("{} member", level.role()),
        }
    }

    /// The token bits this caller carries, or `None` for a Keycloak identity.
    fn permissions(self) -> Option<TokenPermissions> {
        let bits = |rm, rd, wm, wd| {
            Some(TokenPermissions {
                read_metadata: rm,
                read_data: rd,
                write_metadata: wm,
                write_data: wd,
            })
        };
        match self {
            Caller::ReadMetaToken => bits(true, false, false, false),
            Caller::ReadDataToken => bits(false, true, false, false),
            Caller::WriteMetaToken => bits(false, false, true, false),
            Caller::WriteDataToken => bits(false, false, false, true),
            Caller::FullToken | Caller::SyncSession | Caller::ScopedToken => {
                bits(true, true, true, true)
            }
            Caller::Anonymous | Caller::Member(_) => None,
        }
    }

    fn needs_keycloak(self) -> bool {
        matches!(self, Caller::Member(_))
    }

    /// Whether the caller's project access is confined, which is what the CRUD scope guard and
    /// `deny_scoped_token` react to. Administrators and unscoped tokens are unrestricted.
    fn is_restricted(self) -> bool {
        matches!(
            self,
            Caller::ScopedToken
                | Caller::Member(Level::Intern)
                | Caller::Member(Level::River)
                | Caller::Member(Level::Manager)
        )
    }
}

// --- The routes ---

/// What a route's scope layer does to a caller whose project access is confined, on top of the
/// capability gate.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Scope {
    /// No scope layer, or a target inside the caller's own project.
    Open,
    /// `deny_scoped_token` is layered: a project-scoped token is refused outright, a granted
    /// member passes (their grants confine them inside the handler).
    DenyScopedToken,
    /// `inject_project_scope` over an entity with no project dimension: a scoped token may not
    /// mutate the shared catalog, a member's role governs.
    GlobalCatalog,
    /// `inject_project_scope` over a project-bound entity whose owning project this row does not
    /// resolve (a create with no owning FK, a delete of a row that is not there): every restricted
    /// principal fails closed.
    UnresolvedProject,
    /// The handler serves a cross-project view and refuses any confined principal, a granted
    /// member as much as a scoped token.
    CrossProject,
}

/// Whether the CRUD scope guard finds a project dimension on an entity. Mirrors
/// `middleware::crud_scope_condition`, which is where the answer is decided.
#[derive(Clone, Copy, Debug, PartialEq)]
enum CrudScope {
    /// The shared catalog and the operational tables: a member's role governs, a scoped token is
    /// refused every mutation.
    Global,
    /// An owning project is resolved from the row or from the create body's foreign key. Neither
    /// probe below names one, so every restricted principal is refused.
    ProjectBound,
    /// A standard curve belongs to an instrument, and an instrument deployed nowhere has no
    /// project dimension at all, so a member's write reaches one. A project-scoped token is
    /// confined to the projects the instrument stood in, in both directions.
    UnboundIsGlobal,
}

struct Route {
    method: &'static str,
    /// The path as `routes/service/mod.rs` declares it, which is what the drift guard matches.
    declared: &'static str,
    /// The path as issued, with fixture ids in place of the parameters.
    path: String,
    body: Option<serde_json::Value>,
    cap: Capability,
    token: TokenAccess,
    scope: Scope,
    /// The handler refuses API tokens itself (it binds to a Keycloak `sub`), so a token that
    /// clears the gate still gets 403.
    keycloak_only_handler: bool,
}

struct Table(Vec<Route>);

impl Table {
    fn new() -> Self {
        Table(Vec::new())
    }

    /// One route. `declared` may carry `{param}` segments; `path` is what gets issued.
    fn add(
        &mut self,
        method: &'static str,
        declared: &'static str,
        path: impl Into<String>,
        cap: Capability,
        token: TokenAccess,
        scope: Scope,
    ) {
        self.0.push(Route {
            method,
            declared,
            path: path.into(),
            body: None,
            cap,
            token,
            scope,
            keycloak_only_handler: false,
        });
    }

    fn with_body(&mut self, body: serde_json::Value) {
        self.0.last_mut().expect("a route to attach to").body = Some(body);
    }

    fn keycloak_only(&mut self) {
        self.0
            .last_mut()
            .expect("a route to attach to")
            .keycloak_only_handler = true;
    }

    /// A group of routes sharing one gate, the shape the router itself is built in.
    fn group(
        &mut self,
        cap: Capability,
        token: TokenAccess,
        scope: Scope,
        routes: &[(&'static str, &'static str)],
    ) {
        for (method, declared) in routes {
            let path = substitute(declared);
            self.add(method, declared, path, cap, token, scope);
            // A body-taking method needs a JSON body, or the extractor answers 415 before the
            // handler's own scope check ever runs.
            if matches!(*method, "POST" | "PATCH" | "PUT") {
                self.with_body(serde_json::json!({}));
            }
        }
    }
}

/// Fixture ids for the path parameters. Every project-bound parameter resolves inside the seed
/// project, so a restricted caller is refused by the capability gate or not at all; the
/// out-of-project half is `scope_confinement_denies_another_projects_row`.
fn substitute(declared: &str) -> String {
    declared
        .replace("{site_id}", SITE1_ID)
        .replace("{project_id}", PROJECT_ID)
        .replace("{resolution}", "hourly")
        .replace("{job_name}", "janitor_service")
        .replace("{tool_name}", "doc")
        .replace("{code}", "TESTPROJ")
        .replace("{version_id}", MISSING_ID)
        .replace("{sensor_id}", MISSING_ID)
        .replace("{event_id}", MISSING_ID)
        .replace("{set_id}", MISSING_ID)
        .replace("{id}", MISSING_ID)
}

/// A well-formed uuid naming nothing, for the row-addressed routes. The gate is a layer, so it
/// answers before the handler ever looks the row up.
const MISSING_ID: &str = "00000000-0000-4000-f000-0000000000ff";

/// Every CrudCrate entity mounted in `routes/service/mod.rs`, with the gate its nest applies.
/// `create` is a body that resolves the owning project where the entity has one, so the scope
/// guard passes and the capability gate is what decides.
struct Entity {
    name: &'static str,
    read: Capability,
    write: Capability,
    token: TokenAccess,
    crud: CrudScope,
    /// The route families the entity mounts, out of `create`, `read`, `update` and `delete`. An
    /// entity whose rows are a projection of something else declares fewer, and a family it does
    /// not mount is not a route at all, so there is no answer for the table to state.
    families: &'static [&'static str],
}

fn entities() -> Vec<Entity> {
    let entity = |name, read, write, token, crud| Entity {
        name,
        read,
        write,
        token,
        crud,
        families: &["create", "read", "update", "delete"],
    };
    let field = |name, crud| {
        entity(
            name,
            Capability::ReadMetadata,
            Capability::WriteFieldMetadata,
            TokenAccess::Same,
            crud,
        )
    };
    let field_data = |name, crud| {
        entity(
            name,
            Capability::ReadData,
            Capability::WriteFieldMetadata,
            TokenAccess::Same,
            crud,
        )
    };
    let sensor = |name, crud| {
        entity(
            name,
            Capability::ReadMetadata,
            Capability::ManageSensors,
            TokenAccess::Same,
            crud,
        )
    };
    let catalog = |name, crud| {
        entity(
            name,
            Capability::ReadMetadata,
            Capability::WriteCatalog,
            TokenAccess::Same,
            crud,
        )
    };
    let inventory = |name| {
        entity(
            name,
            Capability::ReadMetadata,
            Capability::WriteCatalog,
            TokenAccess::Bit(TokenBit::WriteMetadata),
            CrudScope::Global,
        )
    };
    let admin_write = |name, crud| {
        entity(
            name,
            Capability::ReadMetadata,
            Capability::Admin,
            TokenAccess::Bit(TokenBit::WriteMetadata),
            crud,
        )
    };
    let admin_only = |name| {
        entity(
            name,
            Capability::Admin,
            Capability::Admin,
            TokenAccess::Deny,
            CrudScope::Global,
        )
    };
    vec![
        admin_write("projects", CrudScope::Global),
        field("sites", CrudScope::ProjectBound),
        inventory("parameters"),
        catalog("site_parameters", CrudScope::ProjectBound),
        inventory("sensors"),
        sensor("sensor_calibrations", CrudScope::ProjectBound),
        sensor("sensor_deployments", CrudScope::ProjectBound),
        field("standard_curves", CrudScope::UnboundIsGlobal),
        admin_write("derived_parameters", CrudScope::Global),
        admin_write("derived_parameter_sources", CrudScope::Global),
        admin_write("calculation_shared_steps", CrudScope::Global),
        admin_write("parameter_groups", CrudScope::Global),
        admin_write("parameter_group_members", CrudScope::Global),
        catalog("alarm_thresholds", CrudScope::ProjectBound),
        admin_only("tokens"),
        admin_only("sync_service_credentials"),
        // The forensic trail: every API-token request appends one row, nothing else writes one.
        Entity {
            families: &["read"],
            ..admin_only("api_token_audit_logs")
        },
        admin_write("data_streams", CrudScope::ProjectBound),
        field("subprojects", CrudScope::ProjectBound),
        field("notes", CrudScope::ProjectBound),
        catalog("notification_mutes", CrudScope::Global),
        admin_only("notification_logs"),
        // Channel health, `routes(read)`: the sweeper writes it, nothing else, so read is the
        // whole surface.
        Entity {
            families: &["read"],
            ..admin_only("notification_states")
        },
        // The roster, `routes(read)`: a person's own `/notifications/me` is the only writer.
        Entity {
            families: &["read"],
            ..admin_only("notification_subscribers")
        },
        field_data("annotations", CrudScope::ProjectBound),
        catalog("constants", CrudScope::Global),
        catalog("meteoswiss_subscriptions", CrudScope::Global),
        field_data("samples", CrudScope::ProjectBound),
        field_data("collection_events", CrudScope::Global),
        // A job is enqueued by the worker and driven by the rerun and cancel actions below; the
        // CRUD surface is the queue's read side.
        Entity {
            families: &["read"],
            ..admin_write("reprocessing_jobs", CrudScope::Global)
        },
        // The job timeline: a running job appends, nothing else writes one.
        Entity {
            families: &["read"],
            ..admin_write("reprocessing_job_logs", CrudScope::ProjectBound)
        },
        admin_only("sync_services"),
        admin_only("sync_commands"),
        admin_only("sync_events"),
        admin_only("pairing_plans"),
        Entity {
            families: &["read", "update"],
            ..sensor("schedules", CrudScope::Global)
        },
        // The alarm history: the sweeper opens and resolves episodes, nothing else writes one.
        Entity {
            families: &["read"],
            ..field_data("alarm_events", CrudScope::ProjectBound)
        },
        // One row per tool calculation, minted by the run itself.
        Entity {
            families: &["read"],
            ..field_data("tool_runs", CrudScope::ProjectBound)
        },
        // The review queue as rows: a hold is raised by the path that detects it and decided
        // through the named transitions under `/sync/replicate_audit_holds`, so the entity is
        // read-only and carries the sensor gate those transitions carry.
        Entity {
            families: &["read"],
            ..sensor("replicate_audit_holds", CrudScope::Global)
        },
        // The proposed corrections as rows: raised by the windowed ingest, decided through
        // `/sync/change_proposals/decide`, so the entity is read-only under the same gate.
        Entity {
            families: &["read"],
            ..sensor("reading_change_proposals", CrudScope::Global)
        },
        // The readings themselves: written by ingest, batch, grab entry and CSV import, changed
        // through the curation routes, never deleted, so the entity is read-only.
        Entity {
            families: &["read"],
            ..field_data("readings", CrudScope::ProjectBound)
        },
        // The curation ledger: append-only, every writer a curation path with its own rules
        // (ADR 0008). `GET /readings/decisions` is one reading's history; this is the table.
        Entity {
            families: &["read"],
            ..field_data("reading_decisions", CrudScope::Global)
        },
        // Append-only: every writer is a side effect of the change it records.
        Entity {
            families: &["read"],
            ..field("change_audit_entries", CrudScope::Global)
        },
        // Written by the ingest pass, deleted only by the janitor's age prune.
        Entity {
            families: &["read"],
            ..field("ingest_receipts", CrudScope::Global)
        },
    ]
}

fn table() -> Table {
    let mut t = Table::new();

    for e in entities() {
        // The read arm and the write arm of the method-aware CRUD gate. A create carrying no
        // owning FK is what makes the project-bound entities fail closed for a restricted caller.
        t.add(
            "GET",
            "crud:list",
            format!("/api/{}", e.name),
            e.read,
            TokenAccess::Same,
            Scope::Open,
        );
        // A create naming no owning foreign key resolves no project on a project-bound entity,
        // and neither does a delete of a row that is not there.
        let create = match e.crud {
            CrudScope::Global => Scope::GlobalCatalog,
            CrudScope::ProjectBound | CrudScope::UnboundIsGlobal => Scope::UnresolvedProject,
        };
        // Deleting by an id that is not there is refused for every restricted caller, including on
        // the entities whose unbound rows a member may write: the row filter answers "not there"
        // for a row it excludes and for a row that does not exist alike.
        let delete = match e.crud {
            CrudScope::Global => Scope::GlobalCatalog,
            CrudScope::ProjectBound | CrudScope::UnboundIsGlobal => Scope::UnresolvedProject,
        };
        if e.families.contains(&"create") {
            t.add(
                "POST",
                "crud:create",
                format!("/api/{}", e.name),
                e.write,
                e.token,
                create,
            );
            t.with_body(serde_json::json!({}));
        }
        if e.families.contains(&"delete") {
            t.add(
                "DELETE",
                "crud:delete",
                format!("/api/{}/{MISSING_ID}", e.name),
                e.write,
                e.token,
                delete,
            );
        }
    }

    // /projects and /sites carry read views beside their CRUD.
    t.group(
        Capability::ReadMetadata,
        TokenAccess::Same,
        Scope::Open,
        &[("GET", "/api/projects/{project_id}/sites")],
    );
    t.group(
        Capability::ReadData,
        TokenAccess::Same,
        Scope::Open,
        &[
            ("GET", "/api/sites/{site_id}/readings"),
            ("GET", "/api/sites/{site_id}/aggregates/{resolution}"),
            ("GET", "/api/sites/{site_id}/status_events"),
            ("GET", "/api/sites/{site_id}/alarms"),
            ("GET", "/api/sites/{site_id}/annotations"),
            ("GET", "/api/sites/{site_id}/export/summary"),
            ("GET", "/api/sites/{site_id}/export/replicates"),
            ("GET", "/api/sites/{site_id}/export/sensor-vs-grab"),
            ("GET", "/api/sites/{site_id}/statistics"),
            ("GET", "/api/sites/{site_id}/sensor_identity"),
            ("GET", "/api/sites/{site_id}/last_curve"),
        ],
    );
    t.group(
        Capability::ReadMetadata,
        TokenAccess::Same,
        Scope::Open,
        &[
            ("GET", "/api/sites/{site_id}/parameters"),
            ("GET", "/api/sites/{site_id}/detail"),
        ],
    );

    t.group(
        Capability::ReadMetadata,
        TokenAccess::Same,
        Scope::Open,
        &[
            ("GET", "/api/streams/{id}/stats"),
            ("GET", "/api/streams/{id}/preview"),
            ("GET", "/api/streams/{id}/receipts"),
        ],
    );

    t.group(
        Capability::ReadData,
        TokenAccess::Same,
        Scope::Open,
        &[
            ("GET", "/api/sensors/{id}/readings"),
            ("GET", "/api/sensors/{id}/deployment_bands"),
            ("GET", "/api/sensor_calibrations/{id}/window"),
            ("GET", "/api/sensors/{id}/curve_usage"),
            ("GET", "/api/standard_curves/{id}/usage"),
        ],
    );
    t.group(
        Capability::ReadData,
        TokenAccess::Same,
        Scope::CrossProject,
        &[("GET", "/api/instruments/overview")],
    );

    t.group(
        Capability::Admin,
        TokenAccess::Bit(TokenBit::WriteMetadata),
        Scope::DenyScopedToken,
        &[
            ("POST", "/api/streams/register"),
            ("POST", "/api/standard_curves/register"),
            ("POST", "/api/sensors/register"),
            ("POST", "/api/sensors/proposals"),
            ("POST", "/api/notes/register"),
            ("POST", "/api/annotations/register"),
            ("POST", "/api/streams/retag"),
            ("POST", "/api/streams/{id}/import"),
            ("POST", "/api/streams/{id}/pair"),
            ("POST", "/api/streams/{id}/unpair"),
        ],
    );

    t.group(
        Capability::ReadMetadata,
        TokenAccess::Same,
        Scope::Open,
        &[("GET", "/api/sensors/{sensor_id}/adopt_suggestions")],
    );
    t.group(
        Capability::ManageSensors,
        TokenAccess::Same,
        Scope::DenyScopedToken,
        &[
            ("POST", "/api/sensors/{sensor_id}/adopt"),
            ("POST", "/api/sensors/retag_frequency"),
            ("POST", "/api/actions/swap"),
            ("POST", "/api/actions/rollback_deployment"),
        ],
    );

    t.group(
        Capability::WriteData,
        TokenAccess::Same,
        Scope::Open,
        &[
            ("POST", "/api/ingest"),
            ("POST", "/api/ingest/status_events"),
            ("POST", "/api/readings/batch"),
            ("POST", "/api/status_events/batch"),
            ("POST", "/api/readings/import_csv"),
            ("POST", "/api/readings/import_csv/chunk"),
            ("POST", "/api/collection_events/stage"),
            ("POST", "/api/collection_events/stage_many"),
            ("POST", "/api/collection_events/{id}/recompute"),
            ("POST", "/api/actions/event_audit"),
            ("POST", "/api/actions/event_recompute"),
            ("POST", "/api/readings/edits/preview"),
            ("POST", "/api/readings/edits"),
            ("POST", "/api/readings/edits/{id}/rollback"),
            ("POST", "/api/readings/edits/sets/{set_id}/rollback"),
            ("PATCH", "/api/readings/flag"),
            ("PATCH", "/api/readings/unflag"),
            ("PATCH", "/api/readings/flag_range"),
            ("PATCH", "/api/readings/unflag_range"),
        ],
    );

    t.group(
        Capability::EnterFieldData,
        TokenAccess::Same,
        Scope::Open,
        &[("POST", "/api/grab_samples")],
    );

    t.group(
        Capability::WriteData,
        TokenAccess::Same,
        Scope::DenyScopedToken,
        &[
            ("POST", "/api/actions/reconcile_alarms"),
            ("POST", "/api/actions/backfill_attribution"),
            ("POST", "/api/actions/backfill_calibrations"),
            ("POST", "/api/alarms/{event_id}/acknowledge"),
            ("DELETE", "/api/alarms/{event_id}/acknowledge"),
        ],
    );
    // `require_named_target`: an action that names no site would reach outside a confined
    // caller's projects, so a member granted one project is refused it as firmly as a scoped
    // token. `backfill_attribution` is not here: its `all` flag defaults to false, which names a
    // selection.
    t.group(
        Capability::WriteData,
        TokenAccess::Same,
        Scope::CrossProject,
        &[
            ("POST", "/api/actions/reprocess_all"),
            ("POST", "/api/actions/rebuild_alarm_events"),
        ],
    );
    t.add(
        "POST",
        "/api/actions/compute_derived",
        "/api/actions/compute_derived",
        Capability::WriteData,
        TokenAccess::Same,
        Scope::CrossProject,
    );
    t.with_body(serde_json::json!({ "site_timestamps": [] }));

    t.group(
        Capability::ReadData,
        TokenAccess::Same,
        Scope::Open,
        &[
            ("POST", "/api/actions/preview_derived"),
            ("POST", "/api/readings/sample_preview"),
            ("GET", "/api/alarms/active"),
            ("GET", "/api/alarms/summary"),
            ("GET", "/api/alarms/events"),
            ("GET", "/api/alarms/thresholds"),
            ("GET", "/api/events"),
            ("GET", "/api/reprocessing_jobs/{id}/logs"),
            ("GET", "/api/tools"),
            ("GET", "/api/calculations/closure"),
            ("GET", "/api/derived_parameters/{id}/dependents"),
            ("POST", "/api/tools/{tool_name}/calculate"),
            ("POST", "/api/readings/seasonal_check"),
            ("GET", "/api/readings/provenance"),
            ("GET", "/api/readings/ledger"),
            ("GET", "/api/readings/decisions"),
            ("POST", "/api/readings/edits/inspect"),
            ("GET", "/api/tool_runs/{id}/reload"),
            ("GET", "/api/sites/{id}/visits"),
            ("GET", "/api/visits"),
            ("GET", "/api/collection_events/{id}/detail"),
            ("GET", "/api/actions/curation_drift"),
        ],
    );

    t.group(
        Capability::ReadMetadata,
        TokenAccess::Same,
        Scope::Open,
        &[
            ("GET", "/api/search"),
            ("GET", "/api/version"),
            ("GET", "/api/actions/undeclared_sd_estimators"),
            ("GET", "/api/parameter_groups/{id}/definition"),
            ("GET", "/api/change_audit"),
            ("GET", "/api/schedules"),
            ("GET", "/api/schedules/{job_name}"),
            ("GET", "/api/schedules/{job_name}/audit"),
            ("GET", "/api/schedules/runnable"),
            ("GET", "/api/meteoswiss/stations"),
        ],
    );

    // The write counterparts of these two are behind `deny_scoped_token`, so the enumeration
    // feeding them is refused a scoped token as well.
    t.group(
        Capability::ReadMetadata,
        TokenAccess::Same,
        Scope::DenyScopedToken,
        &[
            ("GET", "/api/actions/backfill_candidates"),
            ("GET", "/api/actions/calibration_candidates"),
        ],
    );

    t.group(
        Capability::ManageSensors,
        TokenAccess::Same,
        Scope::DenyScopedToken,
        &[
            ("POST", "/api/actions/sensor_calibrations/{id}/recalculate"),
            ("POST", "/api/actions/reprocess"),
            ("POST", "/api/actions/derived_parameters/{id}/recompute"),
            ("POST", "/api/actions/invalidate_public_config/{code}"),
            ("POST", "/api/reprocessing_jobs/{id}/rerun"),
            ("POST", "/api/reprocessing_jobs/{id}/cancel"),
            ("PATCH", "/api/schedules/{job_name}"),
            ("POST", "/api/schedules/{job_name}/run_now"),
            ("POST", "/api/site_parameters/{id}/declare_sd_estimator"),
            ("POST", "/api/actions/retag_sd_estimator"),
            ("POST", "/api/parameter_groups/{id}/intermediates"),
            ("POST", "/api/sensor_calibrations/{id}/retire"),
            ("POST", "/api/sensor_calibrations/{id}/unretire"),
            ("POST", "/api/standard_curves/{id}/retire"),
            ("POST", "/api/standard_curves/{id}/unretire"),
            ("POST", "/api/sites/{site_id}/parameter_groups"),
        ],
    );

    t.group(
        Capability::Admin,
        TokenAccess::Bit(TokenBit::WriteMetadata),
        Scope::DenyScopedToken,
        &[
            ("POST", "/api/actions/merge_parameters"),
            ("POST", "/api/actions/merge_site_parameters"),
        ],
    );

    t.group(
        Capability::ReadMetadata,
        TokenAccess::Same,
        Scope::Open,
        &[
            ("GET", "/api/sync/pairing-plans"),
            ("GET", "/api/sync/pairing-plans/{id}"),
            ("GET", "/api/sync/pairing-plans/{id}/site-metadata"),
            ("GET", "/api/sync/pairing-plans/{id}/instruments"),
            ("GET", "/api/sync/unpaired-summary"),
        ],
    );

    t.group(
        Capability::Admin,
        TokenAccess::Bit(TokenBit::WriteMetadata),
        Scope::DenyScopedToken,
        &[
            ("POST", "/api/sync/services/{id}/commands"),
            ("POST", "/api/sync/services/{id}/revoke"),
            ("POST", "/api/sync/pairing-plans"),
            ("PATCH", "/api/sync/pairing-plans/{id}"),
            ("POST", "/api/sync/pairing-plans/{id}/apply"),
            ("POST", "/api/sync/pairing-plans/{id}/supersede"),
            ("POST", "/api/sync/pairing-plans/{id}/revert"),
        ],
    );

    t.group(
        Capability::ManageSensors,
        TokenAccess::Same,
        Scope::DenyScopedToken,
        &[
            ("GET", "/api/sync/replicate_audit_holds"),
            ("POST", "/api/sync/replicate_audit_holds/{id}/acknowledge"),
            ("POST", "/api/sync/replicate_audit_holds/{id}/resolve"),
            ("POST", "/api/sync/replicate_audit_holds/{id}/reopen"),
            ("POST", "/api/sync/replicate_audit_holds/acknowledge_bulk"),
            ("GET", "/api/sync/replicate_reconciliation/duplicate_slots"),
            ("GET", "/api/sync/replicate_reconciliation/candidates"),
            ("POST", "/api/sync/replicate_reconciliation"),
            ("POST", "/api/sync/change_proposals/decide"),
        ],
    );

    t.group(
        Capability::Admin,
        TokenAccess::Bit(TokenBit::WriteMetadata),
        Scope::DenyScopedToken,
        &[("POST", "/api/sync/replicate_reconciliation/delete")],
    );

    t.group(
        Capability::Admin,
        TokenAccess::Deny,
        Scope::Open,
        &[
            ("POST", "/api/sync/credentials"),
            ("POST", "/api/sync/credentials/{id}/revoke"),
            ("POST", "/api/readings/detach"),
            ("POST", "/api/readings/return"),
        ],
    );

    t.add(
        "GET",
        "/api/users",
        "/api/users",
        Capability::Admin,
        TokenAccess::Deny,
        Scope::Open,
    );
    t.group(
        Capability::Admin,
        TokenAccess::Deny,
        Scope::Open,
        &[
            ("GET", "/api/users/search"),
            ("GET", "/api/users/{id}"),
            ("DELETE", "/api/users/{id}"),
            ("POST", "/api/users/{id}/roles"),
            ("GET", "/api/users/{id}/grants"),
            ("GET", "/api/roles"),
        ],
    );

    t.group(
        Capability::Admin,
        TokenAccess::Deny,
        Scope::Open,
        &[
            ("GET", "/api/tool_scripts"),
            ("POST", "/api/tool_scripts"),
            ("GET", "/api/tool_scripts/{id}"),
            ("PATCH", "/api/tool_scripts/{id}"),
            ("POST", "/api/tool_scripts/inspect"),
            ("POST", "/api/tool_scripts/{id}/versions"),
            ("GET", "/api/tool_scripts/{id}/versions/{version_id}"),
            (
                "POST",
                "/api/tool_scripts/{id}/versions/{version_id}/validate",
            ),
            (
                "POST",
                "/api/tool_scripts/{id}/versions/{version_id}/activate",
            ),
            ("GET", "/api/tool_scripts/{id}/activations"),
        ],
    );

    t.group(
        Capability::Admin,
        TokenAccess::Deny,
        Scope::Open,
        &[
            ("GET", "/api/notifications/health"),
            ("GET", "/api/notifications/deliveries"),
            ("POST", "/api/notifications/health/refresh"),
        ],
    );

    t.group(
        Capability::Admin,
        TokenAccess::Deny,
        Scope::Open,
        &[
            ("POST", "/api/tokens/{id}/revoke"),
            ("POST", "/api/tokens/{id}/rotate"),
            ("GET", "/api/tokens/{id}/usage"),
            ("GET", "/api/api_token_audit_logs/distinct/status_codes"),
        ],
    );

    // Self-service surfaces: the gate admits a token, the handler does not.
    for (method, declared) in [
        ("GET", "/api/notifications/me"),
        ("PATCH", "/api/notifications/me"),
        ("PUT", "/api/notifications/me/subscriptions"),
        ("GET", "/api/notifications/me/push"),
        ("POST", "/api/notifications/me/push/ping"),
        ("GET", "/api/notifications/channels"),
    ] {
        t.add(
            method,
            declared,
            declared.to_string(),
            Capability::ReadData,
            TokenAccess::Same,
            Scope::Open,
        );
        if method != "GET" {
            t.with_body(if declared.ends_with("/subscriptions") {
                serde_json::json!({ "subscriptions": [] })
            } else {
                serde_json::json!({})
            });
        }
        t.keycloak_only();
    }
    for declared in ["/api/me", "/api/me/sites"] {
        t.add(
            "GET",
            declared,
            declared.to_string(),
            Capability::ReadMetadata,
            TokenAccess::Same,
            Scope::Open,
        );
        t.keycloak_only();
    }

    t
}

// --- What each pair should return ---

#[derive(Debug, PartialEq)]
enum Outcome {
    Unauthenticated,
    Forbidden,
    Allowed,
}

fn expected(route: &Route, caller: Caller) -> Outcome {
    if caller == Caller::Anonymous {
        return Outcome::Unauthenticated;
    }
    let passes_gate = match caller.permissions() {
        Some(permissions) => token_allows(&permissions, route.cap, route.token),
        None => {
            let Caller::Member(level) = caller else {
                unreachable!("only a member has no token bits")
            };
            keycloak_allows(&[level.role()], route.cap)
        }
    };
    if !passes_gate {
        return Outcome::Forbidden;
    }
    if caller.permissions().is_some() && route.keycloak_only_handler {
        return Outcome::Forbidden;
    }
    if caller.is_restricted() {
        let confined = match route.scope {
            Scope::Open => false,
            Scope::DenyScopedToken | Scope::GlobalCatalog => caller == Caller::ScopedToken,
            Scope::UnresolvedProject => route.method != "GET",
            Scope::CrossProject => true,
        };
        if confined {
            return Outcome::Forbidden;
        }
    }
    Outcome::Allowed
}

// --- Issuing ---

/// The status of one request, without draining the body: `/api/events` is an open SSE stream and
/// collecting it would never return. A gate answers in the headers either way.
async fn status_of(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
    token: Option<&str>,
) -> u16 {
    use tower::ServiceExt;
    let mut req = axum::http::Request::builder().method(method).uri(path);
    if let Some(token) = token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let req = match body {
        Some(json) => req
            .header("Content-Type", "application/json")
            .body(axum::body::Body::from(json.to_string()))
            .unwrap(),
        None => req.body(axum::body::Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(req).await.expect("the router answers");
    response.status().as_u16()
}

fn check(route: &Route, caller: Caller, actual: u16, crashes: &mut Vec<String>) {
    let want = expected(route, caller);
    let label = format!("{} {} as {}", route.method, route.path, caller.name());
    match want {
        Outcome::Unauthenticated => assert_eq!(actual, 401, "[{label}] expected 401, got {actual}"),
        // A write naming no project the caller can be checked against is refused, and the row
        // filter decides how. A create is validated inside the write's own transaction, so an
        // entity with a required column answers 422 before there is a row to reject, and one that
        // builds a row outside the scope answers 403. A row the filter excludes is a row that is
        // not there, so an update or a delete of it answers 404. None of the three writes anything.
        Outcome::Forbidden if route.scope == Scope::UnresolvedProject => {
            assert!(
                matches!(actual, 403 | 404 | 422),
                "[{label}] expected the write to be refused, got {actual}"
            );
        }
        Outcome::Forbidden => assert_eq!(actual, 403, "[{label}] expected 403, got {actual}"),
        // An admitted caller may still fail for a non-auth reason (400 on a junk body, 404 on a
        // missing row). Only the auth boundary is under test, but a 500 is not a refusal and not
        // an answer: it is the handler crashing on an admitted caller, which this table is the
        // one place to see.
        Outcome::Allowed => {
            assert!(
                !(401..=403).contains(&actual),
                "[{label}] expected to pass the gate, got {actual}"
            );
            // The tool run answers 503 when the R runner is unreachable, which is the deliberate
            // answer under a profile that excludes the runner rather than a crash.
            let runner_excluded = actual == 503
                && route.declared == "/api/tools/{tool_name}/calculate"
                && !crate::common::profile::selected()
                    .covers(crate::common::profile::Service::ToolsRunner);
            if actual >= 500 && !runner_excluded {
                crashes.push(format!(
                    "[{label}] passed the gate and crashed with {actual}"
                ));
            }
        }
    }
}

// --- The suites ---

async fn principals(
    db: &sea_orm::DatabaseConnection,
    with_keycloak: bool,
) -> Vec<(Caller, Option<String>)> {
    let mut out: Vec<(Caller, Option<String>)> = vec![
        (Caller::Anonymous, None),
        (
            Caller::ReadMetaToken,
            Some(crate::common::seed_token_read_metadata_only(db).await),
        ),
        (
            Caller::ReadDataToken,
            Some(crate::common::seed_token_read_data_only(db).await),
        ),
        (
            Caller::WriteMetaToken,
            Some(crate::common::seed_token_write_metadata_only(db).await),
        ),
        (
            Caller::WriteDataToken,
            Some(crate::common::seed_token_write_data_only(db).await),
        ),
        (
            Caller::FullToken,
            Some(crate::common::seed_token_full(db).await),
        ),
        (
            Caller::SyncSession,
            Some(crate::common::seed_sync_session_token(db).await.0),
        ),
        (
            Caller::ScopedToken,
            Some(
                crate::common::seed_api_token(
                    db,
                    crate::common::full_permissions(),
                    Some(PROJECT_ID),
                )
                .await,
            ),
        ),
    ];
    if with_keycloak {
        for level in [
            Level::Intern,
            Level::River,
            Level::Manager,
            Level::Administrator,
        ] {
            let user = level.username();
            if level != Level::Administrator {
                ensure_realm_user(user, user, &[&level.role().to_string()]).await;
                grant_project(db, &keycloak_user_id(user).await, PROJECT_ID).await;
            }
            out.push((
                Caller::Member(level),
                Some(get_keycloak_jwt(user, user).await),
            ));
        }
    }
    out
}

#[tokio::test]
#[serial]
async fn every_route_answers_every_caller_as_the_policy_says() {
    let with_keycloak = keycloak_reachable().await;
    if !with_keycloak {
        eprintln!(
            "SKIP levels: keycloak unreachable (start the dev stack, or set TEST_KEYCLOAK_URL)"
        );
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = build_test_app_with_keycloak_admin(db.clone()).await;
    let callers = principals(&db, with_keycloak).await;

    let mut crashes = Vec::new();
    for route in &table().0 {
        for (caller, token) in &callers {
            if caller.needs_keycloak() && !with_keycloak {
                continue;
            }
            let actual = status_of(
                &app,
                route.method,
                &route.path,
                route.body.as_ref(),
                token.as_deref(),
            )
            .await;
            check(route, *caller, actual, &mut crashes);
        }
    }
    assert!(
        crashes.is_empty(),
        "routes that crash for an admitted caller:\n  {}",
        crashes.join("\n  ")
    );
}

/// The second row of every project-bound route: the same request against a project the caller was
/// never granted. Both restricted principals are driven, a granted member and a scoped token,
/// because the two are confined by different machinery.
#[tokio::test]
#[serial]
async fn scope_confinement_denies_another_projects_row() {
    if !keycloak_reachable().await {
        eprintln!("SKIP: keycloak unreachable");
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    crate::common::db::exec(
        &db,
        &format!("INSERT INTO projects (id, name) VALUES ('{OTHER_PROJECT_ID}', 'Outside')"),
    )
    .await;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO sites (id, name, project_id) VALUES ('{OTHER_SITE_ID}', 'Outside Site', '{OTHER_PROJECT_ID}')"
        ),
    )
    .await;
    let app = build_test_app_with_keycloak_admin(db.clone()).await;

    ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    grant_project(&db, &keycloak_user_id("manager1").await, PROJECT_ID).await;
    let member = get_keycloak_jwt("manager1", "manager1").await;
    let scoped =
        crate::common::seed_api_token(&db, crate::common::full_permissions(), Some(PROJECT_ID))
            .await;

    // Two rows per route: the granted project answers, the other one does not.
    let reads = [
        "/api/sites/{site}/readings",
        "/api/sites/{site}/aggregates/hourly?start=2025-01-01T00:00:00Z&end=2025-01-02T00:00:00Z",
        "/api/sites/{site}/status_events",
        "/api/sites/{site}/parameters",
        "/api/sites/{site}/detail",
        "/api/sites/{site}/visits",
    ];
    for template in reads {
        for (label, token) in [("granted member", &member), ("scoped token", &scoped)] {
            let inside = template.replace("{site}", SITE1_ID);
            let outside = template.replace("{site}", OTHER_SITE_ID);
            let s = status_of(&app, "GET", &inside, None, Some(token)).await;
            assert!(
                !(401..=403).contains(&s),
                "[{label}] {inside} is inside the grant, got {s}"
            );
            let s = status_of(&app, "GET", &outside, None, Some(token)).await;
            assert!(
                s == 403 || s == 404,
                "[{label}] {outside} is outside the grant, got {s}"
            );
        }
    }

    // A write into another project's row, through the CRUD scope guard.
    for (label, token) in [("granted member", &member), ("scoped token", &scoped)] {
        let outside = serde_json::json!({ "site_id": OTHER_SITE_ID, "text": "not mine" });
        let s = status_of(&app, "POST", "/api/notes", Some(&outside), Some(token)).await;
        assert_eq!(s, 403, "[{label}] a note in another project is refused");
        let inside = serde_json::json!({ "site_id": SITE1_ID, "text": "mine" });
        let s = status_of(&app, "POST", "/api/notes", Some(&inside), Some(token)).await;
        assert!(
            !(401..=403).contains(&s),
            "[{label}] a note in the granted project lands, got {s}"
        );
    }

    // Reads through CRUD list are filtered rather than refused: the other project's site is absent.
    for (label, token) in [("granted member", &member), ("scoped token", &scoped)] {
        let (_, body) = crate::common::get_with_token(&app, "/api/sites", token).await;
        assert!(
            !body.contains(OTHER_SITE_ID),
            "[{label}] the site list leaked another project's row"
        );
    }

    // A name resolves the same way an id does: a site addressed by name outside the grant is no
    // more reachable than one addressed by uuid.
    let s = status_of(
        &app,
        "GET",
        "/api/sites/Outside%20Site/detail",
        None,
        Some(&scoped),
    )
    .await;
    assert!(s == 403 || s == 404, "a foreign site by name answers {s}");

    // The unconfined half: an unscoped key reaches both projects, so the denials above are
    // confinement rather than the row being unreachable.
    let unscoped =
        crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    for site in [SITE1_ID, OTHER_SITE_ID] {
        let s = status_of(
            &app,
            "GET",
            &format!("/api/sites/{site}/detail"),
            None,
            Some(&unscoped),
        )
        .await;
        assert_eq!(s, 200, "an unscoped key reaches {site}");
    }

    // The public API is unauthenticated and sees no scope at all.
    let (s, _) = crate::common::get(&app, "/api/public").await;
    assert_eq!(s, 200, "public discovery answers without auth");
}

/// The table is only a guarantee while it names every route. Each source file below is read for
/// its `.route("…")` literals and every one must appear, so a route added without a row fails
/// here instead of going unprobed.
#[test]
fn every_registered_route_has_a_row() {
    // Routes whose absence from the table is deliberate, with the reason.
    let exempt = [
        // Mounted under /api/sync by the sync control plane, whose auth is the enrollment
        // credential and the session token, not the capability gates this table covers.
        "/api/sync/enroll",
        "/api/sync/heartbeat",
        "/api/sync/commands/{id}",
        "/api/sync/events",
        "/api/sync/events/{id}",
        // The tool authoring surface's runner-backed drafts, exercised by the tools theme against
        // a live runner; the gate is the same require_admin as its siblings in the table.
        "/api/tool_scripts/draft_run",
        "/api/tool_scripts/{id}/formulas/draft_run",
        // Both send: a probe here would try to deliver. Their gates are the require_read_data and
        // require_admin the sibling rows in the table already carry.
        "/api/notifications/me/push/test",
        "/api/notifications/test-send",
    ];

    let declared: std::collections::HashSet<String> =
        table().0.iter().map(|r| r.declared.to_string()).collect();
    let (routes, mut missing) = registered_routes(&ROUTE_SOURCES);
    for (file, full) in routes {
        if exempt.contains(&full.as_str()) {
            continue;
        }
        // /api/users/ is declared as "/" under its nest.
        let normalised = full.trim_end_matches('/').to_string();
        if !declared.contains(&full) && !declared.contains(&normalised) {
            missing.push(format!("{file}: {full}"));
        }
    }
    assert!(
        missing.is_empty(),
        "routes registered with no row in the permission matrix:\n  {}",
        missing.join("\n  ")
    );
}

/// Every entity nested in the service router, so a new CrudCrate mount cannot slip past the table.
#[test]
fn every_nested_entity_has_a_row() {
    let text = std::fs::read_to_string("src/routes/service/mod.rs").expect("read service router");
    let known: std::collections::HashSet<&str> = entities().iter().map(|e| e.name).collect();
    let mut missing = Vec::new();
    for nest in nest_literals(&text) {
        let name = nest.trim_start_matches('/');
        // The plain-handler groups nest under /sync; their routes are in the table individually.
        // /sync and /users are plain-handler groups; their routes are in the table one by one.
        if name == "sync" || name == "users" || known.contains(name) {
            continue;
        }
        missing.push(name.to_string());
    }
    assert!(
        missing.is_empty(),
        "entities nested with no row in the permission matrix: {missing:?}"
    );
}

/// The first string literal of every `.route(` call in a source file.
/// The converse guarantee: a row for a route that no longer exists probes a 404 and says nothing,
/// so the table drifts into claiming gates on surfaces that are gone. Rows under a nested entity
/// router are not `.route` literals, so only the paths whose prefix one of the source files owns
/// are checked here.
#[test]
fn every_row_names_a_route_that_still_exists() {
    let (routes, drift) = registered_routes(&ROUTE_SOURCES);
    let registered: std::collections::HashSet<String> =
        routes.into_iter().map(|(_, full)| full).collect();
    // Only the prefixes whose paths are route literals in these files are checked: the rest are
    // served by nested routers, so their absence here means nothing. `/api/actions/` is one of
    // them, and a row naming an action route that was deleted is how this test earns its keep.
    let gone: Vec<&str> = table()
        .0
        .iter()
        .map(|r| r.declared)
        .filter(|d| d.starts_with("/api/sync/") || d.starts_with("/api/actions/"))
        .filter(|d| !registered.contains(*d) && !registered.contains(d.trim_end_matches('/')))
        .filter(|d| {
            ![
                "/api/sync/enroll",
                "/api/sync/heartbeat",
                "/api/sync/events/{id}",
            ]
            .contains(d)
        })
        .collect();
    let findings: Vec<String> = drift
        .into_iter()
        .chain(gone.iter().map(|d| (*d).to_string()))
        .collect();
    assert!(
        findings.is_empty(),
        "rows in the permission matrix whose route is gone:\n  {}",
        findings.join("\n  ")
    );
}

/// The source files whose `.route("…")` literals the two coverage scans read, each with the prefix
/// it is mounted under. A component collapse that moves the literals moves the entry here too.
const ROUTE_SOURCES: [(&str, &str); 5] = [
    ("src/routes/service/mod.rs", "/api"),
    ("src/routes/private/sync/views.rs", "/api/sync"),
    ("src/routes/private/projects/views.rs", "/api/projects"),
    ("src/routes/private/sites/views.rs", "/api/sites"),
    ("src/routes/private/api_tokens/views.rs", "/api"),
];

/// Every route [`ROUTE_SOURCES`] declares, paired with the file it came from, and the entries that
/// could not be read. A moved file is drift in this test's own data, so it is returned as a finding
/// beside the missing rows rather than ending the scan: a scan that stops at the first unreadable
/// entry reports nothing about the routes it did read, and every route added meanwhile goes
/// unprobed without the suite saying so.
fn registered_routes(sources: &[(&str, &str)]) -> (Vec<(String, String)>, Vec<String>) {
    let mut routes = Vec::new();
    let mut drift = Vec::new();
    for (file, prefix) in sources {
        let Ok(text) = std::fs::read_to_string(file) else {
            drift.push(format!(
                "{file}: unreadable, and it holds the `.route` literals for {prefix}"
            ));
            continue;
        };
        for path in route_literals(&text) {
            let full = if path == "/" {
                (*prefix).to_string()
            } else {
                format!("{prefix}{path}")
            };
            routes.push(((*file).to_string(), full));
        }
    }
    (routes, drift)
}

/// A source entry that no longer resolves is drift in the table's own data, so the scan reports it
/// and keeps reading the rest. It used to panic on the first one, which left every route in the
/// remaining files unchecked for as long as the entry stood.
#[test]
fn test_a_moved_source_is_reported_as_drift_and_stops_no_other_scan() {
    let sources = [
        ("src/routes/private/admin/users.rs", "/api/users"),
        ("src/routes/service/mod.rs", "/api"),
    ];
    let (routes, drift) = registered_routes(&sources);
    assert_eq!(drift.len(), 1, "{drift:?}");
    assert!(
        drift[0].starts_with("src/routes/private/admin/users.rs:"),
        "{drift:?}"
    );
    assert!(
        routes.iter().any(|(_, full)| full == "/api/version"),
        "the readable source is still scanned"
    );
}

fn route_literals(text: &str) -> Vec<String> {
    literals_after(text, ".route(")
}

fn nest_literals(text: &str) -> Vec<String> {
    literals_after(text, ".nest(")
}

fn literals_after(text: &str, marker: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(marker) {
        rest = &rest[at + marker.len()..];
        let Some(open) = rest.find('"') else { break };
        // Anything other than whitespace before the quote means the path is not a literal here.
        if rest[..open].chars().any(|c| !c.is_whitespace()) {
            continue;
        }
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else { break };
        out.push(after[..close].to_string());
        rest = &after[close + 1..];
    }
    out
}
