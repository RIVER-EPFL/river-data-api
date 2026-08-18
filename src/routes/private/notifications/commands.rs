//! Bot command handlers. Each returns the reply text; authorization and routing live in `bot`.
//!
//! Everything is answered from the database directly (the bot runs in-process), so there are no HTTP
//! self-calls. Site and parameter arguments are matched case-insensitively by name/code.

use axum::Json;
use axum::extract::State;
use chrono::{Duration, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::authz::AccessScope;
use crate::common::middleware::ProjectScope;
use crate::routes::private::readings::grab_samples::{
    GrabSampleReading, GrabSampleRequest, insert_grab_samples,
};

use crate::routes::private::alarms::thresholds::{
    ResolvedThreshold, resolve_thresholds_sql, severity_of_range,
};

use super::keyboard::{self, Action, Button};
use super::messages::severity_label;
use super::{Reply, plot_args};
use crate::common::{plot, series_query};

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;

/// Concurrent chart renders. Rendering is the one CPU-bound thing the bot does, so a burst of taps
/// must not occupy every blocking thread the runtime has.
static RENDER_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// A `uuid[]` bind for confining a `sites` query to the caller's scope: SQL NULL when unrestricted
/// (administrator, no filter), else the granted project ids (an empty array matches nothing). Pair
/// with a `(... IS NULL OR <site>.project_id = ANY(...))` fragment so one bind serves both cases.
fn scope_projects_bind(scope: &AccessScope) -> sea_orm::Value {
    use sea_orm::sea_query::ArrayType;
    match scope.project_ids() {
        None => sea_orm::Value::Array(ArrayType::Uuid, None),
        Some(ids) => sea_orm::Value::Array(
            ArrayType::Uuid,
            Some(Box::new(
                ids.into_iter().map(sea_orm::Value::from).collect(),
            )),
        ),
    }
}

pub fn help() -> String {
    "River Data bot commands. Send one on its own and I'll show you a list to tap.\n\
     \n\
     /status, alarm summary\n\
     /alarms, open alarms\n\
     /sites, sites by project\n\
     /latest [site], latest reading per parameter\n\
     /thresholds [site], configured thresholds\n\
     /server, sync service status\n\
     /battery [site], voltage and depletion forecast\n\
     /grab <site> <param> <value> [more], submit a grab sample\n\
     /mute [site] [param] [days], suppress alerts\n\
     /unmute [site] [param]\n\
     /muted, active mutes, each with a button to lift it\n\
     /ping, liveness check\n\
     /help, this list\n\
     \n\
     Charts:\n\
     /plot, pick a site and parameter from a list\n\
     /plot <site>, every parameter at that site in one image\n\
     /plot <site> <param> [window], e.g. /plot Saxon turbidity 6h\n\
     /1d /3d /7d /30d <site> <param>, fixed windows\n\
     Windows: 90m, 6h, 2d, 1w, 3mo (up to 3y).\n\
     Every chart carries buttons to change window or parameter.\n\
     For a site whose name has spaces, separate with commas:\n\
     /plot Les Dailles, depth, 2d\n\
     \n\
     /grab is the one command that still needs typing: it carries a number.\n\
     \n\
     Setup:\n\
     /start <code>, link this chat to your account"
        .to_string()
}

pub fn ping() -> String {
    "pong".to_string()
}

enum SiteMatch {
    One(Uuid, String),
    NotFound,
    Ambiguous(Vec<String>),
}

async fn resolve_site(
    db: &DatabaseConnection,
    scope: &AccessScope,
    arg: &str,
) -> Result<SiteMatch, sea_orm::DbErr> {
    // Out-of-scope sites resolve to NotFound (indistinguishable from a bad name), so a member can't
    // even probe for a site outside their granted projects.
    let projects = scope_projects_bind(scope);
    if let Ok(id) = Uuid::parse_str(arg) {
        let row = db
            .query_one(Statement::from_sql_and_values(
                PG,
                "SELECT name FROM sites WHERE id = $1 \
                 AND ($2::uuid[] IS NULL OR project_id = ANY($2))",
                [id.into(), projects],
            ))
            .await?;
        return Ok(match row {
            Some(r) => SiteMatch::One(id, r.try_get("", "name")?),
            None => SiteMatch::NotFound,
        });
    }
    let rows = db
        .query_all(Statement::from_sql_and_values(
            PG,
            "SELECT id, name FROM sites WHERE name ILIKE $1 \
             AND ($2::uuid[] IS NULL OR project_id = ANY($2)) ORDER BY name",
            [format!("%{arg}%").into(), projects],
        ))
        .await?;
    match rows.len() {
        0 => Ok(SiteMatch::NotFound),
        1 => Ok(SiteMatch::One(
            rows[0].try_get("", "id")?,
            rows[0].try_get("", "name")?,
        )),
        _ => {
            let names = rows
                .iter()
                .filter_map(|r| r.try_get::<String>("", "name").ok())
                .collect();
            Ok(SiteMatch::Ambiguous(names))
        }
    }
}

fn ambiguous_reply(kind: &str, names: &[String]) -> String {
    format!(
        "Multiple {kind} match, be more specific: {}",
        names.join(", ")
    )
}

/// The window a parameter button opens with, and the one an overview covers.
const BUTTON_WINDOW: &str = "24h";
/// At most this many panels in a site overview: past six, each panel is too small to read.
const MAX_OVERVIEW_PANELS: usize = 6;
/// Sites and parameters offered as buttons. Telegram renders a long keyboard badly.
const MAX_SITE_BUTTONS: u32 = 30;
const MAX_PARAMETER_BUTTONS: u32 = 24;
/// Mutes offered as one-tap undo. Each is a full-width row, so this is lower than the pickers.
const MAX_MUTE_BUTTONS: usize = 12;

/// The sites the caller may see. `make` decides what tapping one does, so a single query serves
/// every picker rather than each command growing its own.
async fn site_buttons(
    db: &DatabaseConnection,
    scope: &AccessScope,
    make: impl Fn(String) -> Action,
) -> Result<Vec<Button>, sea_orm::DbErr> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            PG,
            "SELECT id, name FROM sites \
             WHERE ($1::uuid[] IS NULL OR project_id = ANY($1)) ORDER BY name LIMIT $2",
            [
                scope_projects_bind(scope),
                i64::from(MAX_SITE_BUTTONS).into(),
            ],
        ))
        .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let id: Uuid = r.try_get("", "id").ok()?;
            Some(Button {
                text: r.try_get("", "name").ok()?,
                data: make(keyboard::short(id)).encode(),
            })
        })
        .collect())
}

/// `(id, name, units)` for the parameters configured at a site, measurements first.
async fn site_parameter_list(
    db: &DatabaseConnection,
    site_id: Uuid,
    limit: u32,
) -> Result<Vec<(Uuid, String, Option<String>)>, sea_orm::DbErr> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            PG,
            "SELECT p.id AS id, p.name AS name, \
                    COALESCE(NULLIF(sp.display_units, ''), p.default_units) AS units \
             FROM site_parameters sp JOIN parameters p ON p.id = sp.parameter_id \
             WHERE sp.site_id = $1 \
             ORDER BY (p.category = 'measurement') DESC, p.name LIMIT $2",
            [site_id.into(), i64::from(limit).into()],
        ))
        .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some((
                r.try_get("", "id").ok()?,
                r.try_get("", "name").ok()?,
                r.try_get::<Option<String>>("", "units").ok().flatten(),
            ))
        })
        .collect())
}

/// The parameters configured at one site. `make` receives `(site, parameter)` already encoded.
async fn parameter_buttons(
    db: &DatabaseConnection,
    site_id: Uuid,
    make: impl Fn(&str, String) -> Action,
) -> Result<Vec<Button>, sea_orm::DbErr> {
    let site = keyboard::short(site_id);
    Ok(site_parameter_list(db, site_id, MAX_PARAMETER_BUTTONS)
        .await?
        .into_iter()
        .map(|(id, name, _)| Button {
            text: name,
            data: make(&site, keyboard::short(id)).encode(),
        })
        .collect())
}

/// Tapping a parameter opens its chart. The default for every picker that is not part of muting.
fn view_at_button_window(site: &str, parameter: String) -> Action {
    Action::View {
        site: site.to_string(),
        parameter,
        window: BUTTON_WINDOW.to_string(),
    }
}

/// The row under a chart: the other windows, then a way back out.
fn chart_keyboard(site_id: Uuid, parameter_id: Uuid, window: &str) -> keyboard::Keyboard {
    let site = keyboard::short(site_id);
    vec![
        keyboard::window_row(&site, &keyboard::short(parameter_id), window),
        vec![
            Button {
                text: "All parameters".to_string(),
                data: Action::Parameters(site.clone()).encode(),
            },
            Button {
                text: "Sites".to_string(),
                data: Action::Sites.encode(),
            },
        ],
    ]
}

/// A site picker, used when no site was named and when one could not be resolved.
pub async fn sites_menu(
    db: &DatabaseConnection,
    scope: &AccessScope,
    lead: &str,
    make: impl Fn(String) -> Action,
) -> Reply {
    match site_buttons(db, scope, make).await {
        Ok(buttons) if buttons.is_empty() => {
            Reply::Text("No sites are visible to your account.".to_string())
        }
        Ok(buttons) => Reply::Menu {
            text: lead.to_string(),
            keyboard: keyboard::rows(buttons, 2),
        },
        Err(e) => Reply::Text(db_error(&e)),
    }
}

/// A parameter picker for one site. Answers "no parameter matches" with the ones that do.
pub async fn parameters_menu(
    db: &DatabaseConnection,
    site_id: Uuid,
    lead: &str,
    make: impl Fn(&str, String) -> Action,
) -> Reply {
    match parameter_buttons(db, site_id, make).await {
        Ok(buttons) if buttons.is_empty() => {
            Reply::Text(format!("{lead}\nThis site has no parameters configured."))
        }
        Ok(buttons) => {
            let mut keys = keyboard::rows(buttons, 2);
            keys.push(vec![Button {
                text: "Sites".to_string(),
                data: Action::Sites.encode(),
            }]);
            Reply::Menu {
                text: lead.to_string(),
                keyboard: keys,
            }
        }
        Err(e) => Reply::Text(db_error(&e)),
    }
}

pub async fn sites(db: &DatabaseConnection, scope: &AccessScope) -> Reply {
    let listing = sites_text(db, scope).await;
    match site_buttons(db, scope, Action::Overview).await {
        Ok(buttons) if !buttons.is_empty() => Reply::Menu {
            text: format!("{listing}\n\nTap a site for its charts."),
            keyboard: keyboard::rows(buttons, 2),
        },
        _ => Reply::Text(listing),
    }
}

async fn sites_text(db: &DatabaseConnection, scope: &AccessScope) -> String {
    let rows = match db
        .query_all(Statement::from_sql_and_values(
            PG,
            "SELECT COALESCE(pr.name, '(no project)') AS project, s.name AS site \
             FROM sites s LEFT JOIN projects pr ON pr.id = s.project_id \
             WHERE ($1::uuid[] IS NULL OR s.project_id = ANY($1)) \
             ORDER BY project, site",
            [scope_projects_bind(scope)],
        ))
        .await
    {
        Ok(r) => r,
        Err(e) => return db_error(&e),
    };
    if rows.is_empty() {
        return "No sites configured.".to_string();
    }
    let mut out = String::from("Sites:\n");
    let mut current = String::new();
    for r in &rows {
        let project: String = r.try_get("", "project").unwrap_or_default();
        let site: String = r.try_get("", "site").unwrap_or_default();
        if project != current {
            out.push_str(&format!("{project}:\n"));
            current = project;
        }
        out.push_str(&format!("  • {site}\n"));
    }
    out.trim_end().to_string()
}

pub async fn alarms(db: &DatabaseConnection, scope: &AccessScope) -> String {
    let rows = match db
        .query_all(Statement::from_sql_and_values(
            PG,
            "SELECT s.name AS site, p.name AS param, ae.severity AS severity, \
                    ae.last_value AS value, p.default_units AS units \
             FROM alarm_events ae \
             JOIN sites s ON s.id = ae.site_id \
             JOIN parameters p ON p.id = ae.parameter_id \
             WHERE ae.resolved_at IS NULL \
               AND ($1::uuid[] IS NULL OR s.project_id = ANY($1)) \
             ORDER BY ae.severity DESC, s.name",
            [scope_projects_bind(scope)],
        ))
        .await
    {
        Ok(r) => r,
        Err(e) => return db_error(&e),
    };
    if rows.is_empty() {
        return "✅ No open alarms.".to_string();
    }
    let mut out = format!("🔴 Open alarms ({}):\n", rows.len());
    for r in &rows {
        let site: String = r.try_get("", "site").unwrap_or_default();
        let param: String = r.try_get("", "param").unwrap_or_default();
        let severity: i16 = r.try_get("", "severity").unwrap_or(0);
        let value: f64 = r.try_get("", "value").unwrap_or(0.0);
        let units: String = r.try_get("", "units").unwrap_or_default();
        out.push_str(&format!(
            "{site} / {param}: {value:.2} {units} ({})\n",
            severity_label(severity)
        ));
    }
    out.trim_end().to_string()
}

pub async fn status(db: &DatabaseConnection, scope: &AccessScope) -> String {
    let rows = match db
        .query_all(Statement::from_sql_and_values(
            PG,
            "SELECT ae.severity, COUNT(*) AS n FROM alarm_events ae \
             JOIN sites s ON s.id = ae.site_id \
             WHERE ae.resolved_at IS NULL \
               AND ($1::uuid[] IS NULL OR s.project_id = ANY($1)) \
             GROUP BY ae.severity",
            [scope_projects_bind(scope)],
        ))
        .await
    {
        Ok(r) => r,
        Err(e) => return db_error(&e),
    };
    let mut alarm = 0i64;
    let mut warning = 0i64;
    for r in &rows {
        let sev: i16 = r.try_get("", "severity").unwrap_or(0);
        let n: i64 = r.try_get("", "n").unwrap_or(0);
        if sev >= 2 {
            alarm += n;
        } else {
            warning += n;
        }
    }
    let total = alarm + warning;
    if total == 0 {
        "✅ All clear, no open alarms.".to_string()
    } else {
        format!("{total} open ({alarm} alarm, {warning} warning). Use /alarms for detail.")
    }
}

/// A site named on the command line, or the picker when none was named and when the name given
/// resolves to something other than exactly one site. Naming a site is a shortcut past the menu,
/// never the only way through it.
async fn site_or_picker(
    db: &DatabaseConnection,
    scope: &AccessScope,
    arg: &str,
    lead: &str,
    make: impl Fn(String) -> Action,
) -> Result<(Uuid, String), Reply> {
    if arg.is_empty() {
        return Err(sites_menu(db, scope, lead, make).await);
    }
    match resolve_site(db, scope, arg).await {
        Ok(SiteMatch::One(id, name)) => Ok((id, name)),
        Ok(SiteMatch::NotFound) => Err(sites_menu(
            db,
            scope,
            &format!("No site matches \"{arg}\". Pick one:"),
            make,
        )
        .await),
        Ok(SiteMatch::Ambiguous(names)) => Err(sites_menu(
            db,
            scope,
            &format!("{}\nPick one:", ambiguous_reply("sites", &names)),
            make,
        )
        .await),
        Err(e) => Err(Reply::Text(db_error(&e))),
    }
}

pub async fn latest(db: &DatabaseConnection, scope: &AccessScope, arg: &str) -> Reply {
    match site_or_picker(
        db,
        scope,
        arg,
        "Latest readings at which site?",
        Action::Latest,
    )
    .await
    {
        Ok((site_id, site_name)) => Reply::Text(latest_at(db, site_id, &site_name).await),
        Err(reply) => reply,
    }
}

async fn latest_at(db: &DatabaseConnection, site_id: Uuid, site_name: &str) -> String {
    let rows = match db
        .query_all(Statement::from_sql_and_values(
            PG,
            "SELECT DISTINCT ON (p.id) p.name AS param, p.default_units AS units, \
                    COALESCE(smp.mean, r.calibrated_value, r.raw_value) AS value, r.time AS time \
             FROM readings r JOIN parameters p ON p.id = r.parameter_id \
             LEFT JOIN samples smp ON smp.id = r.sample_id \
             WHERE r.site_id = $1 AND r.replicate_index = 0 \
             ORDER BY p.id, r.time DESC",
            [site_id.into()],
        ))
        .await
    {
        Ok(r) => r,
        Err(e) => return db_error(&e),
    };
    if rows.is_empty() {
        return format!("No readings yet for {site_name}.");
    }
    let mut out = format!("Latest at {site_name}:\n");
    for r in &rows {
        let param: String = r.try_get("", "param").unwrap_or_default();
        let units: String = r.try_get("", "units").unwrap_or_default();
        let value: f64 = r.try_get("", "value").unwrap_or(0.0);
        out.push_str(&format!("{param}: {value:.2} {units}\n"));
    }
    out.trim_end().to_string()
}

/// With no site, the global defaults, and a picker to reach any one site's resolved thresholds.
/// The no-argument listing is kept rather than replaced: the global tier is not reachable any other
/// way.
pub async fn thresholds(db: &DatabaseConnection, scope: &AccessScope, arg: &str) -> Reply {
    if arg.is_empty() {
        let listing = thresholds_at(db, None).await;
        return match site_buttons(db, scope, Action::Thresholds).await {
            Ok(buttons) if !buttons.is_empty() => Reply::Menu {
                text: format!("{listing}\n\nTap a site for the thresholds in force there."),
                keyboard: keyboard::rows(buttons, 2),
            },
            _ => Reply::Text(listing),
        };
    }
    match site_or_picker(
        db,
        scope,
        arg,
        "Thresholds at which site?",
        Action::Thresholds,
    )
    .await
    {
        Ok(site) => Reply::Text(thresholds_at(db, Some(site)).await),
        Err(reply) => reply,
    }
}

async fn thresholds_at(db: &DatabaseConnection, site: Option<(Uuid, String)>) -> String {
    let (site_filter, header) = match &site {
        Some((id, name)) => (Some(*id), format!("Thresholds at {name}:")),
        None => (None, "Global default thresholds:".to_string()),
    };
    let stmt = match site_filter {
        // Site branch: the same three-tier resolution `GET /api/alarms/thresholds` reports, so a
        // slot whose bounds come from the parameter defaults is not reported as unconfigured.
        Some(id) => Statement::from_string(
            PG,
            format!(
                "SELECT p.name AS param, p.default_units AS units, r.warning_min, r.warning_max, \
                        r.alarm_min, r.alarm_max \
                 FROM ({resolved}) r JOIN parameters p ON p.id = r.parameter_id \
                 ORDER BY p.name",
                resolved = resolve_thresholds_sql(Some(id), None)
            ),
        ),
        // No-arg branch: the configured global rows only. The resolution engine is defined per
        // active `(site, parameter)` slot, so it cannot express a site-less listing; a global
        // tier that also falls back to the parameter defaults needs the engine to grow that
        // shape rather than a second ladder here.
        None => Statement::from_string(
            PG,
            "SELECT p.name AS param, p.default_units AS units, at.warning_min, at.warning_max, \
                    at.alarm_min, at.alarm_max \
             FROM alarm_thresholds at JOIN parameters p ON p.id = at.parameter_id \
             WHERE at.site_id IS NULL ORDER BY p.name"
                .to_string(),
        ),
    };
    let rows = match db.query_all(stmt).await {
        Ok(r) => r,
        Err(e) => return db_error(&e),
    };
    if rows.is_empty() {
        return format!("{header}\n(none configured)");
    }
    let mut out = format!("{header}\n");
    for r in &rows {
        let param: String = r.try_get("", "param").unwrap_or_default();
        let units: String = r.try_get("", "units").unwrap_or_default();
        let fmt = |v: Option<f64>| v.map_or("–".to_string(), |x| format!("{x:.1}"));
        let wmin: Option<f64> = r.try_get("", "warning_min").ok().flatten();
        let wmax: Option<f64> = r.try_get("", "warning_max").ok().flatten();
        let amin: Option<f64> = r.try_get("", "alarm_min").ok().flatten();
        let amax: Option<f64> = r.try_get("", "alarm_max").ok().flatten();
        out.push_str(&format!(
            "{param}: warn [{}, {}] alarm [{}, {}] {units}\n",
            fmt(wmin),
            fmt(wmax),
            fmt(amin),
            fmt(amax)
        ));
    }
    out.trim_end().to_string()
}

pub async fn server(db: &DatabaseConnection) -> String {
    let rows = match db
        .query_all(Statement::from_string(
            PG,
            "SELECT instance_id, service_type, status, last_heartbeat, last_error \
             FROM sync_services ORDER BY instance_id"
                .to_string(),
        ))
        .await
    {
        Ok(r) => r,
        Err(e) => return db_error(&e),
    };
    if rows.is_empty() {
        return "No sync services registered.".to_string();
    }
    let mut out = String::from("Sync services:\n");
    for r in &rows {
        let instance: String = r.try_get("", "instance_id").unwrap_or_default();
        let service_type: String = r.try_get("", "service_type").unwrap_or_default();
        let status: String = r.try_get("", "status").unwrap_or_default();
        let last_error: Option<String> = r.try_get("", "last_error").ok().flatten();
        out.push_str(&format!("{service_type}/{instance}: {status}"));
        if let Some(err) = last_error.filter(|e| !e.is_empty()) {
            out.push_str(&format!(" (last error: {err})"));
        }
        out.push('\n');
    }
    if let Ok(Some(row)) = db
        .query_one(Statement::from_string(
            PG,
            "SELECT COUNT(*) AS n FROM sync_events \
             WHERE status = 'failed' AND started_at > NOW() - INTERVAL '24 hours'"
                .to_string(),
        ))
        .await
    {
        let n: i64 = row.try_get("", "n").unwrap_or(0);
        out.push_str(&format!("Failed sync events (24h): {n}"));
    }
    out.trim_end().to_string()
}

/// With no site, every site the caller can see, which is the view worth having. A named site that
/// does not resolve falls back to the picker.
pub async fn battery(
    db: &DatabaseConnection,
    scope: &AccessScope,
    arg: &str,
    cutoff_volts: f64,
) -> Reply {
    if arg.is_empty() {
        return Reply::Text(battery_at(db, scope, None, cutoff_volts).await);
    }
    match site_or_picker(db, scope, arg, "Battery at which site?", Action::Battery).await {
        Ok(site) => Reply::Text(battery_at(db, scope, Some(site), cutoff_volts).await),
        Err(reply) => reply,
    }
}

async fn battery_at(
    db: &DatabaseConnection,
    scope: &AccessScope,
    site: Option<(Uuid, String)>,
    cutoff_volts: f64,
) -> String {
    let battery_param = match db
        .query_one(Statement::from_string(
            PG,
            "SELECT id FROM parameters \
             WHERE category = 'device_health' AND (code ILIKE '%batt%' OR name ILIKE '%batt%') \
             ORDER BY (name ILIKE 'battery') DESC LIMIT 1"
                .to_string(),
        ))
        .await
    {
        Ok(Some(row)) => match row.try_get::<Uuid>("", "id") {
            Ok(id) => id,
            Err(e) => return db_error(&e),
        },
        Ok(None) => return "No battery parameter configured.".to_string(),
        Err(e) => return db_error(&e),
    };

    let site_filter = site.map(|(id, _)| id);

    let rows = match db
        .query_all(Statement::from_sql_and_values(
            PG,
            "SELECT s.name AS site, \
                (SELECT COALESCE(r2.calibrated_value, r2.raw_value) FROM readings r2 \
                   WHERE r2.site_id = s.id AND r2.parameter_id = $1 AND r2.replicate_index = 0 \
                   ORDER BY r2.time DESC LIMIT 1) AS latest, \
                (SELECT regr_slope(COALESCE(r3.calibrated_value, r3.raw_value), \
                                   EXTRACT(EPOCH FROM r3.time) / 86400.0) FROM readings r3 \
                   WHERE r3.site_id = s.id AND r3.parameter_id = $1 AND r3.replicate_index = 0 \
                     AND r3.time > NOW() - INTERVAL '7 days' \
                     AND EXTRACT(HOUR FROM r3.time) BETWEEN 2 AND 4) AS slope \
             FROM sites s \
             WHERE ($2::uuid IS NULL OR s.id = $2) \
               AND ($3::uuid[] IS NULL OR s.project_id = ANY($3)) \
             ORDER BY s.name",
            [
                battery_param.into(),
                site_filter.into(),
                scope_projects_bind(scope),
            ],
        ))
        .await
    {
        Ok(r) => r,
        Err(e) => return db_error(&e),
    };

    let mut out = String::from("🔋 Battery:\n");
    let mut any = false;
    for r in &rows {
        let site: String = r.try_get("", "site").unwrap_or_default();
        let Some(latest) = r.try_get::<Option<f64>>("", "latest").ok().flatten() else {
            continue;
        };
        any = true;
        let slope: Option<f64> = r.try_get("", "slope").ok().flatten();
        let forecast = match slope {
            Some(s) if s < -1e-6 && latest > cutoff_volts => {
                let days = (latest - cutoff_volts) / -s;
                format!(", ~{days:.0}d to {cutoff_volts:.1}V")
            }
            Some(s) => format!(", trend {s:+.3}V/day"),
            None => String::new(),
        };
        out.push_str(&format!("{site}: {latest:.2}V{forecast}\n"));
    }
    if !any {
        return "No battery readings found.".to_string();
    }
    out.trim_end().to_string()
}

async fn resolve_parameter(
    db: &DatabaseConnection,
    arg: &str,
) -> Result<Result<(Uuid, String), String>, sea_orm::DbErr> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            PG,
            "SELECT id, name FROM parameters WHERE code ILIKE $1 OR name ILIKE $1 ORDER BY name",
            [format!("%{arg}%").into()],
        ))
        .await?;
    Ok(match rows.len() {
        0 => Err(format!("No parameter matches \"{arg}\".")),
        1 => Ok((rows[0].try_get("", "id")?, rows[0].try_get("", "name")?)),
        _ => {
            let names: Vec<String> = rows
                .iter()
                .filter_map(|r| r.try_get::<String>("", "name").ok())
                .collect();
            Err(ambiguous_reply("parameters", &names))
        }
    })
}

/// Resolve `<site> <param> [days]` for the mute commands.
async fn resolve_mute_target(
    db: &DatabaseConnection,
    scope: &AccessScope,
    args: &str,
) -> Result<Result<(Uuid, String, Uuid, String, Option<i64>), String>, sea_orm::DbErr> {
    let mut toks: Vec<&str> = args.split_whitespace().collect();
    let days = toks.last().and_then(|t| t.parse::<i64>().ok());
    if days.is_some() {
        toks.pop();
    }
    if toks.len() != 2 {
        return Ok(Err(
            "Usage: /mute <site> <param> [days], or send /mute on its own to pick one.".to_string(),
        ));
    }
    // Administrator-only, so this scope is unrestricted today. Threading it anyway keeps the
    // confinement on the same footing as every other command, rather than on the role gate alone.
    let (site_id, site_name) = match resolve_site(db, scope, toks[0]).await? {
        SiteMatch::One(id, name) => (id, name),
        SiteMatch::NotFound => return Ok(Err(format!("No site matches \"{}\".", toks[0]))),
        SiteMatch::Ambiguous(names) => return Ok(Err(ambiguous_reply("sites", &names))),
    };
    let (param_id, param_name) = match resolve_parameter(db, toks[1]).await? {
        Ok(p) => p,
        Err(msg) => return Ok(Err(msg)),
    };
    Ok(Ok((site_id, site_name, param_id, param_name, days)))
}

pub async fn mute(
    db: &DatabaseConnection,
    scope: &AccessScope,
    args: &str,
    created_by: &str,
) -> Reply {
    if args.is_empty() {
        return sites_menu(db, scope, "Mute alerts at which site?", Action::MuteParams).await;
    }
    let (site_id, site_name, param_id, param_name, days) =
        match resolve_mute_target(db, scope, args).await {
            Ok(Ok(t)) => t,
            Ok(Err(msg)) => return Reply::Text(msg),
            Err(e) => return Reply::Text(db_error(&e)),
        };
    apply_mute(
        db,
        (site_id, &site_name),
        (param_id, &param_name),
        days,
        created_by,
    )
    .await
}

/// Write one mute and answer with its undo. Shared by the typed command and the button, so both
/// record the same provenance and offer the same way back.
async fn apply_mute(
    db: &DatabaseConnection,
    site: (Uuid, &str),
    parameter: (Uuid, &str),
    days: Option<i64>,
    created_by: &str,
) -> Reply {
    let (site_id, site_name) = site;
    let (param_id, param_name) = parameter;
    let expires_at = days.map(|d| Utc::now() + Duration::days(d));
    let res = db
        .execute(Statement::from_sql_and_values(
            PG,
            "INSERT INTO notification_mutes (site_id, parameter_id, expires_at, created_by) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (site_id, parameter_id) \
             DO UPDATE SET expires_at = EXCLUDED.expires_at, created_by = EXCLUDED.created_by",
            [
                site_id.into(),
                param_id.into(),
                expires_at.into(),
                created_by.into(),
            ],
        ))
        .await;
    if let Err(e) = res {
        return Reply::Text(db_error(&e));
    }
    let window = match days {
        Some(d) => format!("for {d} day(s)"),
        None => "until unmuted".to_string(),
    };
    Reply::Menu {
        text: format!("🔕 Muted {site_name} / {param_name} {window}."),
        keyboard: vec![vec![Button {
            text: "Unmute".to_string(),
            data: Action::UnmuteSet {
                site: keyboard::short(site_id),
                parameter: keyboard::short(param_id),
            }
            .encode(),
        }]],
    }
}

/// The last tap of the mute flow. Ids are re-resolved against the caller's scope, so a stale button
/// cannot reach a site the tapper has since lost.
async fn mute_by_button(
    db: &DatabaseConnection,
    scope: &AccessScope,
    site: &str,
    parameter: &str,
    days: i64,
    created_by: &str,
    expired: &str,
) -> Reply {
    let (site_id, site_name) = match resolve_short_site(db, scope, site).await {
        Ok(Some(s)) => s,
        Ok(None) => return Reply::Text(expired.to_string()),
        Err(e) => return Reply::Text(db_error(&e)),
    };
    let (param_id, param_name) = match resolve_short_parameter(db, parameter).await {
        Ok(Some((id, name, _))) => (id, name),
        Ok(None) => return Reply::Text(expired.to_string()),
        Err(e) => return Reply::Text(db_error(&e)),
    };
    // Zero is the no-expiry choice, not a zero-day mute.
    let days = (days > 0).then_some(days);
    apply_mute(
        db,
        (site_id, &site_name),
        (param_id, &param_name),
        days,
        created_by,
    )
    .await
}

/// With no arguments this is the mute listing, whose every line carries its own Unmute button:
/// choosing what to lift from what is actually muted beats naming it from memory.
pub async fn unmute(db: &DatabaseConnection, scope: &AccessScope, args: &str) -> Reply {
    if args.is_empty() {
        return muted(db).await;
    }
    let toks: Vec<&str> = args.split_whitespace().collect();
    if toks.len() != 2 {
        return Reply::Text(
            "Usage: /unmute <site> <param>, or send /unmute on its own to pick one.".to_string(),
        );
    }
    let (site_id, site_name) = match resolve_site(db, scope, toks[0]).await {
        Ok(SiteMatch::One(id, name)) => (id, name),
        Ok(SiteMatch::NotFound) => {
            return Reply::Text(format!("No site matches \"{}\".", toks[0]));
        }
        Ok(SiteMatch::Ambiguous(names)) => return Reply::Text(ambiguous_reply("sites", &names)),
        Err(e) => return Reply::Text(db_error(&e)),
    };
    let (param_id, param_name) = match resolve_parameter(db, toks[1]).await {
        Ok(Ok(p)) => p,
        Ok(Err(msg)) => return Reply::Text(msg),
        Err(e) => return Reply::Text(db_error(&e)),
    };
    Reply::Text(apply_unmute(db, (site_id, &site_name), (param_id, &param_name)).await)
}

/// Lift exactly one mute, keyed on the pair the caller chose.
async fn apply_unmute(
    db: &DatabaseConnection,
    site: (Uuid, &str),
    parameter: (Uuid, &str),
) -> String {
    let (site_id, site_name) = site;
    let (param_id, param_name) = parameter;
    let res = db
        .execute(Statement::from_sql_and_values(
            PG,
            "DELETE FROM notification_mutes WHERE site_id = $1 AND parameter_id = $2",
            [site_id.into(), param_id.into()],
        ))
        .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => format!("🔔 Unmuted {site_name} / {param_name}."),
        Ok(_) => format!("{site_name} / {param_name} was not muted."),
        Err(e) => db_error(&e),
    }
}

type NameMaps = (
    std::collections::HashMap<Uuid, String>,
    std::collections::HashMap<Uuid, String>,
);

/// Site and parameter display names for a set of mutes, keyed by id.
async fn mute_slot_names(
    db: &DatabaseConnection,
    mutes: &[super::mutes_model::Model],
) -> Result<NameMaps, sea_orm::DbErr> {
    use crate::routes::private::{parameters, sites};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let site_rows = sites::Entity::find()
        .filter(sites::Column::Id.is_in(mutes.iter().map(|m| m.site_id).collect::<Vec<_>>()))
        .all(db)
        .await?;
    let parameter_rows = parameters::Entity::find()
        .filter(
            parameters::Column::Id.is_in(mutes.iter().map(|m| m.parameter_id).collect::<Vec<_>>()),
        )
        .all(db)
        .await?;
    Ok((
        site_rows.into_iter().map(|s| (s.id, s.name)).collect(),
        parameter_rows.into_iter().map(|p| (p.id, p.name)).collect(),
    ))
}

/// The mutes currently suppressing delivery, read through the same `in_force` predicate the
/// delivery gate uses, so the listing can never disagree with what is actually muted.
/// Every mute in force, each with the button that lifts it.
pub async fn muted(db: &DatabaseConnection) -> Reply {
    let mutes = match super::mutes_model::in_force_all(db).await {
        Ok(m) => m,
        Err(e) => return Reply::Text(db_error(&e)),
    };
    if mutes.is_empty() {
        return Reply::Text("No active mutes.".to_string());
    }

    let (site_names, param_names) = match mute_slot_names(db, &mutes).await {
        Ok(names) => names,
        Err(e) => return Reply::Text(db_error(&e)),
    };

    let unknown = "(unknown)".to_string();
    let mut rows: Vec<(String, Button)> = mutes
        .iter()
        .map(|m| {
            let site = site_names.get(&m.site_id).unwrap_or(&unknown);
            let param = param_names.get(&m.parameter_id).unwrap_or(&unknown);
            let until = m.expires_at.map_or("permanent".to_string(), |e| {
                e.format("until %Y-%m-%d %H:%M UTC").to_string()
            });
            (
                format!("{site} / {param} ({until})"),
                Button {
                    text: format!("Unmute {site} / {param}"),
                    data: Action::UnmuteSet {
                        site: keyboard::short(m.site_id),
                        parameter: keyboard::short(m.parameter_id),
                    }
                    .encode(),
                },
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    // A long keyboard renders badly, and a silent cut would read as "that is all of them".
    let total = rows.len();
    let shown = rows.len().min(MAX_MUTE_BUTTONS);
    let mut text = format!(
        "Active mutes:\n{}",
        rows.iter()
            .map(|(line, _)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
    if total > shown {
        text.push_str(&format!(
            "\n\nButtons for the first {shown} of {total}; use /unmute <site> <param> for the rest."
        ));
    }
    Reply::Menu {
        text,
        keyboard: keyboard::rows(rows.into_iter().take(shown).map(|(_, b)| b).collect(), 1),
    }
}

/// Lift a mute from its own button. The pair is re-resolved against the caller's scope, so the
/// delete can only ever name a slot the tapper can still see.
async fn unmute_by_button(
    db: &DatabaseConnection,
    scope: &AccessScope,
    site: &str,
    parameter: &str,
    expired: &str,
) -> Reply {
    let (site_id, site_name) = match resolve_short_site(db, scope, site).await {
        Ok(Some(s)) => s,
        Ok(None) => return Reply::Text(expired.to_string()),
        Err(e) => return Reply::Text(db_error(&e)),
    };
    let (param_id, param_name) = match resolve_short_parameter(db, parameter).await {
        Ok(Some((id, name, _))) => (id, name),
        Ok(None) => return Reply::Text(expired.to_string()),
        Err(e) => return Reply::Text(db_error(&e)),
    };
    Reply::Text(apply_unmute(db, (site_id, &site_name), (param_id, &param_name)).await)
}

/// Cancel a pending link code, for when it has been exposed.
///
/// Only unclaimed rows are touched, so this can never unlink a working chat. Returns whether a
/// pending code was actually voided, which is the difference between "that code is now cancelled"
/// and "that code was not valid anyway".
pub async fn void_link_code(db: &DatabaseConnection, code: &str) -> bool {
    if code.is_empty() {
        return false;
    }
    db.execute(Statement::from_sql_and_values(
        PG,
        "DELETE FROM telegram_identities \
         WHERE link_code = $1 AND telegram_chat_id IS NULL",
        [code.into()],
    ))
    .await
    .is_ok_and(|r| r.rows_affected() > 0)
}

/// Claim a link code. Returns `(claimed, reply)`: the flag is what the audit trail records, since
/// a failed claim on a live code is the signal worth keeping.
pub async fn start(
    db: &DatabaseConnection,
    chat_id: i64,
    from_id: Option<i64>,
    code: &str,
    dashboard: Option<&str>,
) -> (bool, String) {
    // A dead end names where to get a code, for anyone who found the bot before the dashboard.
    let where_to_get_one = match dashboard {
        Some(base) => format!("Get one from {}/settings.", base.trim_end_matches('/')),
        None => "Get one from Settings in the River Data dashboard.".to_string(),
    };
    if code.is_empty() {
        return (
            false,
            format!(
                "To connect this chat, send /start followed by your link code. {where_to_get_one}"
            ),
        );
    }
    let res = db
        .query_one(Statement::from_sql_and_values(
            PG,
            "UPDATE telegram_identities \
             SET telegram_chat_id = $1, telegram_user_id = $3, link_code = NULL, \
                 link_code_expires_at = NULL, is_active = TRUE, last_verified_at = NOW(), \
                 last_attested_at = NOW(), updated_at = NOW() \
             WHERE link_code = $2 AND link_code_expires_at > NOW() AND telegram_chat_id IS NULL \
             RETURNING id",
            [chat_id.into(), code.into(), from_id.into()],
        ))
        .await;
    match res {
        Ok(Some(_)) => (
            true,
            "✅ Linked. You'll receive alerts and can use commands, try /help.".to_string(),
        ),
        Ok(None) => (
            false,
            format!("That code is invalid or has expired. {where_to_get_one}"),
        ),
        // Most likely the chat is already linked to another identity (unique constraint).
        Err(_) => (
            false,
            format!("This chat is already linked, or the code is invalid. {where_to_get_one}"),
        ),
    }
}

/// `/grab <site> <param> <value> [more values…]`, submit a grab sample (and its replicates) from
/// the field. Reuses the full grab-sample insert path (stream creation, sample aggregation, alarm
/// reconciliation). When `TELEGRAM_GRAB_FLAG_FOR_REVIEW` is set, the readings are flagged on insert
/// so they're held out of aggregates until a curator reviews them.
pub async fn grab(state: &AppState, scope: &AccessScope, args: &str, sub: &str) -> String {
    let toks: Vec<&str> = args.split_whitespace().collect();
    if toks.len() < 3 {
        return "Usage: /grab <site> <param> <value> [more values…]\nSend /sites for the site names."
            .to_string();
    }
    let (site_arg, param_arg) = (toks[0], toks[1]);
    let mut values = Vec::with_capacity(toks.len() - 2);
    for t in &toks[2..] {
        match t.parse::<f64>() {
            Ok(v) => values.push(v),
            Err(_) => return format!("\"{t}\" is not a number."),
        }
    }

    let (site_id, site_name) = match resolve_site(&state.db, scope, site_arg).await {
        Ok(SiteMatch::One(id, name)) => (id, name),
        Ok(SiteMatch::NotFound) => return format!("No site matches \"{site_arg}\"."),
        Ok(SiteMatch::Ambiguous(names)) => return ambiguous_reply("sites", &names),
        Err(e) => return db_error(&e),
    };
    let (param_id, param_name) = match resolve_parameter(&state.db, param_arg).await {
        Ok(Ok(p)) => p,
        Ok(Err(msg)) => return msg,
        Err(e) => return db_error(&e),
    };

    let now = Utc::now();
    // The Keycloak identity, resolved live for this very message. A Telegram handle is neither
    // stable nor authoritative, so it never becomes provenance.
    let created_by = format!("keycloak:{sub}");
    let readings = values
        .iter()
        .map(|&value| GrabSampleReading {
            parameter_id: param_id,
            sensor_id: None,
            value,
            time: now,
            replicate_index: None,
            standard_curve_id: None,
        })
        .collect();
    let req = GrabSampleRequest {
        site_id,
        created_by: Some(created_by),
        label: None,
        notes: None,
        mode: None,
        dry_run: false,
        readings,
    };

    // The Telegram user's authority was resolved live (anti-backdoor) and gated to River level in the
    // router; the write is confined to the caller's project scope, exactly like HTTP `/grab_samples`.
    match insert_grab_samples(State(state.clone()), ProjectScope(scope.clone()), Json(req)).await {
        Ok(Json(resp)) => {
            let mut reply = format!(
                "✅ Recorded {} value(s) for {site_name} / {param_name}.",
                resp.inserted
            );
            if state.config.telegram_grab_flag_for_review {
                flag_for_review(&state.db, &resp.created_sample_ids).await;
                reply.push_str(" Flagged for review.");
            }
            reply
        }
        Err(e) => {
            tracing::warn!(error = %e, "bot grab insert failed");
            format!("Couldn't record that, is {param_name} configured at {site_name}?")
        }
    }
}

/// Flag exactly the readings this submission created.
///
/// Keyed on the samples the insert reports as *new*, never on (site, parameter, time): a predicate
/// over the slot would also catch a concurrent write landing on the same timestamp, and a bot
/// command must not modify a row it did not create. A re-posted grab reuses its sample and so
/// reports no new ids, which correctly flags nothing.
async fn flag_for_review(db: &DatabaseConnection, sample_ids: &[Uuid]) {
    if sample_ids.is_empty() {
        return;
    }
    let ids = sea_orm::Value::Array(
        sea_orm::sea_query::ArrayType::Uuid,
        Some(Box::new(
            sample_ids
                .iter()
                .copied()
                .map(sea_orm::Value::from)
                .collect(),
        )),
    );
    let res = db
        .execute(Statement::from_sql_and_values(
            PG,
            "UPDATE readings SET is_flagged = TRUE, \
                 flag_reason = 'field submission, pending review' \
             WHERE sample_id = ANY($1::uuid[])",
            [ids],
        ))
        .await;
    if let Err(e) = res {
        tracing::warn!(error = %e, "failed to flag grab sample for review");
    }
}

fn db_error(e: &sea_orm::DbErr) -> String {
    tracing::warn!(error = %e, "bot command query failed");
    "Something went wrong fetching that. Try again shortly.".to_string()
}

// ── Plot ────────────────────────────────────────────────────────────────────────────────────────

/// A resolved `(site, parameter)` pair plus the display names for the chart.
struct PlotTarget {
    site_id: Uuid,
    site_name: String,
    parameter_id: Uuid,
    parameter_name: String,
    units: Option<String>,
}

/// Why a plot request could not be turned into a slot.
///
/// `site` is carried so the reply can offer that site's parameters instead of only naming the
/// failure: the answer to "no parameter matches" is the list of ones that do.
struct TargetError {
    message: String,
    site: Option<(Uuid, String)>,
}

/// Walk the candidate splits and take the first where both sides resolve to exactly one row.
///
/// Ordered parameter-shortest-first by `candidate_splits`, so a site whose name contains a
/// parameter word (`Depth Station`) still resolves correctly. On failure the reply names which
/// side failed, rather than the legacy bot's single "could not generate plot" for everything.
async fn resolve_plot_target(
    db: &DatabaseConnection,
    scope: &AccessScope,
    parsed: &plot_args::ParsedArgs,
) -> Result<Result<PlotTarget, TargetError>, sea_orm::DbErr> {
    let mut last_site_err: Option<String> = None;
    let mut last_param_err: Option<String> = None;
    let mut resolved_site: Option<(Uuid, String)> = None;

    for (site_arg, param_arg) in &parsed.candidates {
        let site = match resolve_site(db, scope, site_arg).await? {
            SiteMatch::One(id, name) => (id, name),
            SiteMatch::NotFound => {
                last_site_err.get_or_insert(format!("No site matches \"{site_arg}\"."));
                continue;
            }
            SiteMatch::Ambiguous(names) => {
                last_site_err.get_or_insert(ambiguous_reply("sites", &names));
                continue;
            }
        };
        resolved_site = Some(site.clone());

        // Try the parameter as typed, then through the legacy alias table. `volt` has been muscle
        // memory for years and matches no parameter code or name on its own.
        let direct = resolve_parameter(db, param_arg).await?;
        let param = match direct {
            Ok(p) => Ok(p),
            Err(err) => match plot_args::alias_for(param_arg) {
                Some(alias) => resolve_parameter(db, alias).await?,
                None => Err(err),
            },
        };
        match param {
            Ok((parameter_id, parameter_name)) => {
                let units = parameter_units(db, parameter_id).await?;
                return Ok(Ok(PlotTarget {
                    site_id: site.0,
                    site_name: site.1,
                    parameter_id,
                    parameter_name,
                    units,
                }));
            }
            Err(err) => {
                last_param_err.get_or_insert(err);
            }
        }
    }

    // Prefer the more specific failure: if a site resolved, the parameter is what went wrong.
    let message = if let (Some((_, site_name)), Some(param_err)) = (&resolved_site, &last_param_err)
    {
        format!("{param_err}\nSite resolved as {site_name}.")
    } else if let Some(site_err) = last_site_err {
        site_err
    } else if let Some(param_err) = last_param_err {
        param_err
    } else {
        "Couldn't read that. Try: /plot <site> <parameter> [window]".to_string()
    };
    Ok(Err(TargetError {
        message,
        site: resolved_site,
    }))
}

async fn parameter_units(
    db: &DatabaseConnection,
    parameter_id: Uuid,
) -> Result<Option<String>, sea_orm::DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            PG,
            "SELECT default_units FROM parameters WHERE id = $1",
            [parameter_id.into()],
        ))
        .await?;
    Ok(row.and_then(|r| {
        r.try_get::<Option<String>>("", "default_units")
            .ok()
            .flatten()
    }))
}

/// Time-ranged notes overlapping the window.
///
/// Overlap, not containment: a note that began before the window and is still open must shade its
/// visible portion rather than disappear.
async fn plot_annotations(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
    start: chrono::DateTime<Utc>,
    end: chrono::DateTime<Utc>,
) -> Vec<plot::AnnotationBand> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            PG,
            "SELECT start_time, end_time, text FROM annotations \
             WHERE site_id = $1 AND parameter_id = $2 AND start_time <= $4 AND end_time >= $3 \
             ORDER BY start_time LIMIT $5",
            [
                site_id.into(),
                parameter_id.into(),
                start.into(),
                end.into(),
                (plot::MAX_ANNOTATION_BANDS as i64).into(),
            ],
        ))
        .await;
    match rows {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|r| {
                Some(plot::AnnotationBand {
                    start: r.try_get("", "start_time").ok()?,
                    end: r.try_get("", "end_time").ok()?,
                    text: r.try_get("", "text").unwrap_or_default(),
                })
            })
            .collect(),
        Err(e) => {
            // A broken annotations query must not cost the user their chart.
            tracing::warn!(error = %e, "plot: annotation lookup failed");
            Vec::new()
        }
    }
}

/// Render a time series as a PNG chart.
///
/// `cmd` is either `plot` (window from the arguments, defaulting to a week) or a legacy window
/// command like `7d`. Returns [`Reply::Text`] for every failure, since the caller has no other way
/// to explain what went wrong.
pub async fn plot(state: &AppState, scope: &AccessScope, cmd: &str, args: &str) -> Reply {
    let db = &state.db;

    // A window can be given without a target, so it is split off before anything is resolved.
    let mut tokens = plot_args::tokenize(args);
    let trailing_window = if tokens.len() > 1
        && tokens
            .last()
            .is_some_and(|t| plot_args::looks_like_window(t))
    {
        tokens.last().cloned()
    } else if tokens.len() == 1 && plot_args::looks_like_window(&tokens[0]) {
        tokens.pop()
    } else {
        None
    };
    let window = match (
        trailing_window.as_deref(),
        plot_args::window_of_command(cmd),
    ) {
        (Some(tok), _) => match plot_args::parse_window(tok) {
            Some(w) => w,
            None => {
                return Reply::Text(format!(
                    "\"{tok}\" isn't a window. Try 6h, 2d, 1w or 30d (up to 3y)."
                ));
            }
        },
        (None, Some(w)) => w,
        (None, None) => Duration::days(plot_args::DEFAULT_WINDOW_DAYS),
    };

    if tokens.is_empty() {
        return sites_menu(db, scope, "Which site?", Action::Overview).await;
    }

    // A site on its own is a complete request: it answers with every parameter at once. Tried
    // before the pair split so "Les Dailles" is read as the site it is, not as site + parameter.
    let named = tokens
        .iter()
        .filter(|t| !plot_args::looks_like_window(t))
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    if let Ok(SiteMatch::One(site_id, site_name)) = resolve_site(db, scope, &named).await {
        // An overview defaults to the last day rather than the plot default of a week: it answers
        // "what is happening at this site", and each panel is a sixth of the usual width.
        let window = if trailing_window.is_some() || plot_args::window_of_command(cmd).is_some() {
            window
        } else {
            Duration::hours(24)
        };
        return site_overview(state, site_id, &site_name, window).await;
    }

    let Some(parsed) = plot_args::parse(args) else {
        return sites_menu(
            db,
            scope,
            &format!("No site matches \"{named}\". Pick one:"),
            Action::Overview,
        )
        .await;
    };

    let target = match resolve_plot_target(db, scope, &parsed).await {
        Ok(Ok(t)) => t,
        Ok(Err(err)) => {
            return match err.site {
                Some((site_id, _)) => {
                    parameters_menu(db, site_id, &err.message, view_at_button_window).await
                }
                None => {
                    sites_menu(
                        db,
                        scope,
                        &format!("{}\nPick a site:", err.message),
                        Action::Overview,
                    )
                    .await
                }
            };
        }
        Err(e) => return Reply::Text(db_error(&e)),
    };

    render_slot(state, &target, window).await
}

/// Draw one resolved slot, with the window switcher under it.
async fn render_slot(state: &AppState, target: &PlotTarget, window: Duration) -> Reply {
    let db = &state.db;
    let end = Utc::now();
    let start = end - window;
    let tier = series_query::tier_for(window);

    let series =
        match series_query::fetch_series(db, target.site_id, target.parameter_id, start, end, tier)
            .await
        {
            Ok(s) => s,
            Err(e) => return Reply::Text(db_error(&e)),
        };

    if series.is_empty() {
        // Naming the latest reading answers the obvious follow-up in the same message. The legacy
        // bot said only "could not generate plot", which conflated this with a typo.
        let latest = latest_reading_time(db, target.site_id, target.parameter_id).await;
        let tail = latest.map_or_else(
            || " No readings recorded at all for this pair.".to_string(),
            |t| format!(" Latest reading: {}.", t.format("%Y-%m-%d %H:%M UTC")),
        );
        return Reply::Menu {
            text: format!(
                "No data for {} at {} in the last {}.{tail}",
                target.parameter_name,
                target.site_name,
                humanize(window)
            ),
            keyboard: chart_keyboard(target.site_id, target.parameter_id, &window_label(window)),
        };
    }

    let (thresholds, resolved) = slot_thresholds(db, target.site_id, target.parameter_id).await;
    let annotations = plot_annotations(db, target.site_id, target.parameter_id, start, end).await;

    let raw_count = series.points.len();
    let points = series_query::decimate(series.points, series_query::MAX_POINTS);
    let envelope: Vec<_> = points
        .iter()
        .filter_map(|p| Some((p.time, p.min?, p.max?)))
        .collect();

    let units_suffix = target
        .units
        .as_deref()
        .filter(|u| !u.is_empty())
        .map_or_else(String::new, |u| format!(" ({u})"));

    let mut spec = plot::PlotSpec::new(
        format!("{}: {}", target.site_name, target.parameter_name),
        format!(
            "last {} · {} · {} point{}",
            humanize(window),
            tier.label(),
            raw_count,
            if raw_count == 1 { "" } else { "s" }
        ),
        format!("{}{units_suffix}", target.parameter_name),
    );
    spec.gap_seconds = tier.gap_seconds();
    spec.severities = point_severities(&points, resolved.as_ref());
    spec.points = points.iter().map(|p| (p.time, p.value)).collect();
    spec.envelope = envelope;
    spec.thresholds = thresholds;
    spec.annotations = annotations;

    let note_count = spec.annotations.len();

    // The render is CPU-bound and the poller handles updates serially, so it must not run on a
    // runtime worker.
    let _slot = RENDER_SLOTS.acquire().await;
    let png = match tokio::task::spawn_blocking(move || plot::render_png(&spec)).await {
        Ok(Ok(png)) => png,
        Ok(Err(plot::PlotError::NoData)) => {
            return Reply::Text(format!(
                "No usable values for {} at {} in that window.",
                target.parameter_name, target.site_name
            ));
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "plot render failed");
            return Reply::Text(
                "Couldn't draw that plot, it's been logged for the team.".to_string(),
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "plot render task panicked");
            return Reply::Text(
                "Couldn't draw that plot, it's been logged for the team.".to_string(),
            );
        }
    };

    let mut caption = format!(
        "{}: {} · last {}",
        target.site_name,
        target.parameter_name,
        humanize(window)
    );
    if note_count > 0 {
        caption.push_str(&format!(
            " · {note_count} note{}",
            if note_count == 1 { "" } else { "s" }
        ));
    }

    Reply::Photo {
        png,
        caption,
        keyboard: Some(chart_keyboard(
            target.site_id,
            target.parameter_id,
            &window_label(window),
        )),
    }
}

/// A slot's thresholds, as chart limit lines and as the classifier behind them.
///
/// A lookup failure degrades to no thresholds: an overlay must never cost the chart.
async fn slot_thresholds(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> (plot::ThresholdLines, Option<ResolvedThreshold>) {
    let resolved = match crate::routes::private::alarms::thresholds::resolve_threshold(
        db,
        site_id,
        parameter_id,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "plot: threshold lookup failed");
            None
        }
    };
    let lines = resolved
        .as_ref()
        .map_or_else(plot::ThresholdLines::default, |t| plot::ThresholdLines {
            warning_min: t.warning_min,
            warning_max: t.warning_max,
            alarm_min: t.alarm_min,
            alarm_max: t.alarm_max,
        });
    (lines, resolved)
}

/// Per-point severity for the chart, from the same ladder the alarm pipeline evaluates.
///
/// A rollup bucket is classified by its extremes rather than its mean, matching `/alarms`: an hour
/// whose peak breached is an alarmed hour even where the average sits inside the limits.
fn point_severities(
    points: &[series_query::SeriesPoint],
    threshold: Option<&ResolvedThreshold>,
) -> Vec<u8> {
    let Some(t) = threshold else {
        return Vec::new();
    };
    points
        .iter()
        .map(|p| {
            let severity = severity_of_range(
                Some(p.min.unwrap_or(p.value)),
                Some(p.max.unwrap_or(p.value)),
                t,
            );
            u8::try_from(severity).unwrap_or(0)
        })
        .collect()
}

/// The button label matching a window, so the switcher can mark the one in view.
fn window_label(window: Duration) -> String {
    keyboard::WINDOW_CHOICES
        .iter()
        .find(|w| plot_args::parse_window(w) == Some(window))
        .map_or_else(String::new, |w| (*w).to_string())
}

/// Every parameter at a site, one panel each, in a single image.
async fn site_overview(
    state: &AppState,
    site_id: Uuid,
    site_name: &str,
    window: Duration,
) -> Reply {
    let db = &state.db;
    let parameters = match site_parameter_list(db, site_id, MAX_OVERVIEW_PANELS as u32).await {
        Ok(p) => p,
        Err(e) => return Reply::Text(db_error(&e)),
    };
    if parameters.is_empty() {
        return Reply::Text(format!("{site_name} has no parameters configured."));
    }

    let end = Utc::now();
    let start = end - window;
    let tier = series_query::tier_for(window);
    let mut specs = Vec::new();
    for (parameter_id, name, units) in &parameters {
        let series =
            match series_query::fetch_series(db, site_id, *parameter_id, start, end, tier).await {
                Ok(s) if !s.is_empty() => s,
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, "overview: series fetch failed");
                    continue;
                }
            };
        let points = series_query::decimate(series.points, series_query::MAX_POINTS);
        let units_suffix = units
            .as_deref()
            .filter(|u| !u.is_empty())
            .map_or_else(String::new, |u| format!(" ({u})"));
        let mut spec = plot::PlotSpec::new(
            format!("{name}{units_suffix}"),
            format!("{} points", points.len()),
            String::new(),
        )
        .into_panel();
        // A panel is classified but carries no limit lines: at a sixth of the width they would
        // crowd the data, and the coloured stretch already says which parameter to look at.
        let (_, resolved) = slot_thresholds(db, site_id, *parameter_id).await;
        spec.gap_seconds = tier.gap_seconds();
        spec.severities = point_severities(&points, resolved.as_ref());
        spec.envelope = points
            .iter()
            .filter_map(|p| Some((p.time, p.min?, p.max?)))
            .collect();
        spec.points = points.iter().map(|p| (p.time, p.value)).collect();
        specs.push(spec);
    }

    if specs.is_empty() {
        return parameters_menu(
            db,
            site_id,
            &format!(
                "No data at {site_name} in the last {}. Pick a parameter to look further back:",
                humanize(window)
            ),
            view_at_button_window,
        )
        .await;
    }

    let panels = specs.len();
    let grid_rows = panels.div_ceil(2).max(1) as u32;
    let height = if panels == 1 { 620 } else { grid_rows * 340 };
    let _slot = RENDER_SLOTS.acquire().await;
    let png = match tokio::task::spawn_blocking(move || {
        plot::render_grid_png(&specs, plot::DEFAULT_WIDTH, height)
    })
    .await
    {
        Ok(Ok(png)) => png,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "overview render failed");
            return Reply::Text(
                "Couldn't draw that overview, it's been logged for the team.".to_string(),
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "overview render task panicked");
            return Reply::Text(
                "Couldn't draw that overview, it's been logged for the team.".to_string(),
            );
        }
    };

    let mut keys = match parameter_buttons(db, site_id, view_at_button_window).await {
        Ok(buttons) => keyboard::rows(buttons, 2),
        Err(e) => {
            tracing::warn!(error = %e, "overview: parameter buttons failed");
            Vec::new()
        }
    };
    keys.push(vec![Button {
        text: "Sites".to_string(),
        data: Action::Sites.encode(),
    }]);

    Reply::Photo {
        png,
        caption: format!(
            "{site_name} · last {} · {panels} parameter{}\nTap one for its own chart.",
            humanize(window),
            if panels == 1 { "" } else { "s" }
        ),
        keyboard: Some(keys),
    }
}

/// Handle a button tap.
///
/// `action` is decoded from data the client sent back, so every id is re-resolved here against the
/// caller's scope: a button is a shortcut, never an authority.
pub async fn callback(state: &AppState, scope: &AccessScope, sub: &str, action: Action) -> Reply {
    let db = &state.db;
    let expired = "That button is out of date. Send /plot to start again.";
    match action {
        Action::Sites => sites_menu(db, scope, "Which site?", Action::Overview).await,
        Action::Parameters(prefix) => match resolve_short_site(db, scope, &prefix).await {
            Ok(Some((site_id, site_name))) => {
                parameters_menu(
                    db,
                    site_id,
                    &format!("{site_name}. Which parameter?"),
                    view_at_button_window,
                )
                .await
            }
            Ok(None) => Reply::Text(expired.to_string()),
            Err(e) => Reply::Text(db_error(&e)),
        },
        Action::Overview(prefix) => match resolve_short_site(db, scope, &prefix).await {
            Ok(Some((site_id, site_name))) => {
                let window =
                    plot_args::parse_window(BUTTON_WINDOW).unwrap_or_else(|| Duration::hours(24));
                site_overview(state, site_id, &site_name, window).await
            }
            Ok(None) => Reply::Text(expired.to_string()),
            Err(e) => Reply::Text(db_error(&e)),
        },
        Action::View {
            site,
            parameter,
            window,
        } => {
            let Some(window) = plot_args::parse_window(&window) else {
                return Reply::Text(expired.to_string());
            };
            let site = match resolve_short_site(db, scope, &site).await {
                Ok(Some(s)) => s,
                Ok(None) => return Reply::Text(expired.to_string()),
                Err(e) => return Reply::Text(db_error(&e)),
            };
            let parameter = match resolve_short_parameter(db, &parameter).await {
                Ok(Some(p)) => p,
                Ok(None) => return Reply::Text(expired.to_string()),
                Err(e) => return Reply::Text(db_error(&e)),
            };
            let target = PlotTarget {
                site_id: site.0,
                site_name: site.1,
                parameter_id: parameter.0,
                parameter_name: parameter.1,
                units: parameter.2,
            };
            render_slot(state, &target, window).await
        }
        Action::LatestSites => sites_menu(db, scope, "Which site?", Action::Latest).await,
        Action::Latest(site) => match resolve_short_site(db, scope, &site).await {
            Ok(Some((site_id, site_name))) => Reply::Text(latest_at(db, site_id, &site_name).await),
            Ok(None) => Reply::Text(expired.to_string()),
            Err(e) => Reply::Text(db_error(&e)),
        },
        Action::ThresholdSites => sites_menu(db, scope, "Which site?", Action::Thresholds).await,
        Action::Thresholds(site) => match resolve_short_site(db, scope, &site).await {
            Ok(Some((site_id, site_name))) => {
                Reply::Text(thresholds_at(db, Some((site_id, site_name))).await)
            }
            Ok(None) => Reply::Text(expired.to_string()),
            Err(e) => Reply::Text(db_error(&e)),
        },
        Action::BatterySites => sites_menu(db, scope, "Which site?", Action::Battery).await,
        Action::Battery(site) => match resolve_short_site(db, scope, &site).await {
            Ok(Some((site_id, site_name))) => Reply::Text(
                battery_at(
                    db,
                    scope,
                    Some((site_id, site_name)),
                    state.config.battery_cutoff_volts,
                )
                .await,
            ),
            Ok(None) => Reply::Text(expired.to_string()),
            Err(e) => Reply::Text(db_error(&e)),
        },
        Action::MuteSites => {
            sites_menu(db, scope, "Mute alerts at which site?", Action::MuteParams).await
        }
        Action::MuteParams(site) => match resolve_short_site(db, scope, &site).await {
            Ok(Some((site_id, site_name))) => {
                parameters_menu(
                    db,
                    site_id,
                    &format!("{site_name}. Mute which parameter?"),
                    |site, parameter| Action::MuteWhen {
                        site: site.to_string(),
                        parameter,
                    },
                )
                .await
            }
            Ok(None) => Reply::Text(expired.to_string()),
            Err(e) => Reply::Text(db_error(&e)),
        },
        Action::MuteWhen { site, parameter } => {
            // Choosing a parameter must not itself mute anything: the length has not been picked.
            match resolve_short_site(db, scope, &site).await {
                Ok(Some((_, site_name))) => match resolve_short_parameter(db, &parameter).await {
                    Ok(Some((_, param_name, _))) => Reply::Menu {
                        text: format!("Mute {site_name} / {param_name} for how long?"),
                        keyboard: vec![keyboard::mute_duration_row(&site, &parameter)],
                    },
                    Ok(None) => Reply::Text(expired.to_string()),
                    Err(e) => Reply::Text(db_error(&e)),
                },
                Ok(None) => Reply::Text(expired.to_string()),
                Err(e) => Reply::Text(db_error(&e)),
            }
        }
        Action::MuteSet {
            site,
            parameter,
            days,
        } => mute_by_button(db, scope, &site, &parameter, days, sub, expired).await,
        Action::Muted => muted(db).await,
        Action::UnmuteSet { site, parameter } => {
            unmute_by_button(db, scope, &site, &parameter, expired).await
        }
    }
}

/// Resolve a button's site id, confined to the caller's scope exactly as a typed name is.
async fn resolve_short_site(
    db: &DatabaseConnection,
    scope: &AccessScope,
    encoded: &str,
) -> Result<Option<(Uuid, String)>, sea_orm::DbErr> {
    let Some(id) = keyboard::from_short(encoded) else {
        return Ok(None);
    };
    let row = db
        .query_one(Statement::from_sql_and_values(
            PG,
            "SELECT id, name FROM sites WHERE id = $1 \
             AND ($2::uuid[] IS NULL OR project_id = ANY($2))",
            [id.into(), scope_projects_bind(scope)],
        ))
        .await?;
    match row {
        Some(r) => Ok(Some((r.try_get("", "id")?, r.try_get("", "name")?))),
        None => Ok(None),
    }
}

async fn resolve_short_parameter(
    db: &DatabaseConnection,
    encoded: &str,
) -> Result<Option<(Uuid, String, Option<String>)>, sea_orm::DbErr> {
    let Some(id) = keyboard::from_short(encoded) else {
        return Ok(None);
    };
    let row = db
        .query_one(Statement::from_sql_and_values(
            PG,
            "SELECT id, name, default_units FROM parameters WHERE id = $1",
            [id.into()],
        ))
        .await?;
    match row {
        Some(r) => Ok(Some((
            r.try_get("", "id")?,
            r.try_get("", "name")?,
            r.try_get::<Option<String>>("", "default_units")?,
        ))),
        None => Ok(None),
    }
}

async fn latest_reading_time(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Option<chrono::DateTime<Utc>> {
    db.query_one(Statement::from_sql_and_values(
        PG,
        "SELECT MAX(time) AS t FROM readings WHERE site_id = $1 AND parameter_id = $2",
        [site_id.into(), parameter_id.into()],
    ))
    .await
    .ok()
    .flatten()
    .and_then(|r| {
        r.try_get::<Option<chrono::DateTime<Utc>>>("", "t")
            .ok()
            .flatten()
    })
}

/// A window as a short phrase for a title and caption.
fn humanize(d: Duration) -> String {
    let mins = d.num_minutes();
    if mins < 60 {
        format!("{mins} min")
    } else if mins < 60 * 48 {
        let h = d.num_hours();
        format!("{h} hour{}", if h == 1 { "" } else { "s" })
    } else if d.num_days() < 60 {
        let days = d.num_days();
        format!("{days} day{}", if days == 1 { "" } else { "s" })
    } else {
        let months = d.num_days() / 30;
        format!("{months} month{}", if months == 1 { "" } else { "s" })
    }
}

/// Render a chart for an already-resolved slot, for the alarm path.
///
/// Takes ids rather than user text because the caller resolved them from an alarm event, not from
/// a command. Every failure degrades to `None`: an alert must go out even when its chart cannot.
/// Scope confinement is the caller's job here: the alert fan-out already applies
/// `project_allowed` per recipient.
pub async fn slot_plot_png(
    state: &AppState,
    site_id: Uuid,
    parameter_id: Uuid,
    window: Duration,
) -> Option<Vec<u8>> {
    let db = &state.db;
    let end = Utc::now();
    let start = end - window;
    let tier = series_query::tier_for(window);

    let series = series_query::fetch_series(db, site_id, parameter_id, start, end, tier)
        .await
        .map_err(|e| tracing::warn!(error = %e, "alarm plot: series fetch failed"))
        .ok()?;
    if series.is_empty() {
        return None;
    }

    let names = db
        .query_one(Statement::from_sql_and_values(
            PG,
            "SELECT s.name AS site, p.name AS param, p.default_units AS units \
             FROM sites s, parameters p WHERE s.id = $1 AND p.id = $2",
            [site_id.into(), parameter_id.into()],
        ))
        .await
        .ok()
        .flatten()?;
    let site_name: String = names.try_get("", "site").ok()?;
    let param_name: String = names.try_get("", "param").ok()?;
    let units: Option<String> = names.try_get("", "units").ok().flatten();

    let (thresholds, resolved) = slot_thresholds(db, site_id, parameter_id).await;

    let raw_count = series.points.len();
    let points = series_query::decimate(series.points, series_query::MAX_POINTS);
    let envelope: Vec<_> = points
        .iter()
        .filter_map(|p| Some((p.time, p.min?, p.max?)))
        .collect();
    let units_suffix = units
        .as_deref()
        .filter(|u| !u.is_empty())
        .map_or_else(String::new, |u| format!(" ({u})"));

    let mut spec = plot::PlotSpec::new(
        format!("{site_name}: {param_name}"),
        format!(
            "last {} · {} · {raw_count} points",
            humanize(window),
            tier.label()
        ),
        format!("{param_name}{units_suffix}"),
    );
    spec.gap_seconds = tier.gap_seconds();
    spec.severities = point_severities(&points, resolved.as_ref());
    spec.points = points.iter().map(|p| (p.time, p.value)).collect();
    spec.envelope = envelope;
    spec.thresholds = thresholds;
    spec.annotations = plot_annotations(db, site_id, parameter_id, start, end).await;

    let _slot = RENDER_SLOTS.acquire().await;
    tokio::task::spawn_blocking(move || plot::render_png(&spec))
        .await
        .map_err(|e| tracing::error!(error = %e, "alarm plot render task panicked"))
        .ok()?
        .map_err(|e| tracing::warn!(error = %e, "alarm plot render failed"))
        .ok()
}
