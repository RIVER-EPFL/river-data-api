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

use crate::routes::private::alarms::thresholds::resolve_thresholds_sql;

use super::messages::severity_label;

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;

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
    "River Data bot commands:\n\
     /status, alarm summary\n\
     /alarms, open alarms\n\
     /stations, sites by project\n\
     /latest <site>, latest reading per parameter\n\
     /thresholds [site], configured thresholds\n\
     /server, sync service status\n\
     /battery [site], voltage and depletion forecast\n\
     /grab <site> <param> <value> [more], submit a grab sample\n\
     /mute <site> <param> [days], suppress alerts\n\
     /unmute <site> <param>\n\
     /muted, active mutes\n\
     /ping, liveness check"
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

pub async fn stations(db: &DatabaseConnection, scope: &AccessScope) -> String {
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
    let mut out = String::from("Stations:\n");
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

pub async fn latest(db: &DatabaseConnection, scope: &AccessScope, arg: &str) -> String {
    if arg.is_empty() {
        return "Usage: /latest <site>".to_string();
    }
    let (site_id, site_name) = match resolve_site(db, scope, arg).await {
        Ok(SiteMatch::One(id, name)) => (id, name),
        Ok(SiteMatch::NotFound) => return format!("No site matches \"{arg}\"."),
        Ok(SiteMatch::Ambiguous(names)) => return ambiguous_reply("sites", &names),
        Err(e) => return db_error(&e),
    };
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

pub async fn thresholds(db: &DatabaseConnection, scope: &AccessScope, arg: &str) -> String {
    let (site_filter, header) = if arg.is_empty() {
        (None, "Global default thresholds:".to_string())
    } else {
        match resolve_site(db, scope, arg).await {
            Ok(SiteMatch::One(id, name)) => (Some(id), format!("Thresholds at {name}:")),
            Ok(SiteMatch::NotFound) => return format!("No site matches \"{arg}\"."),
            Ok(SiteMatch::Ambiguous(names)) => return ambiguous_reply("sites", &names),
            Err(e) => return db_error(&e),
        }
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

pub async fn battery(
    db: &DatabaseConnection,
    scope: &AccessScope,
    arg: &str,
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

    let site_filter = if arg.is_empty() {
        None
    } else {
        match resolve_site(db, scope, arg).await {
            Ok(SiteMatch::One(id, _)) => Some(id),
            Ok(SiteMatch::NotFound) => return format!("No site matches \"{arg}\"."),
            Ok(SiteMatch::Ambiguous(names)) => return ambiguous_reply("sites", &names),
            Err(e) => return db_error(&e),
        }
    };

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
    args: &str,
) -> Result<Result<(Uuid, String, Uuid, String, Option<i64>), String>, sea_orm::DbErr> {
    let mut toks: Vec<&str> = args.split_whitespace().collect();
    let days = toks.last().and_then(|t| t.parse::<i64>().ok());
    if days.is_some() {
        toks.pop();
    }
    if toks.len() != 2 {
        return Ok(Err("Usage: /mute <site> <param> [days]".to_string()));
    }
    // Mute commands are Administrator-only (unrestricted scope), resolved against all sites.
    let (site_id, site_name) = match resolve_site(db, &AccessScope::Unrestricted, toks[0]).await? {
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

pub async fn mute(db: &DatabaseConnection, args: &str, created_by: &str) -> String {
    let (site_id, site_name, param_id, param_name, days) = match resolve_mute_target(db, args).await
    {
        Ok(Ok(t)) => t,
        Ok(Err(msg)) => return msg,
        Err(e) => return db_error(&e),
    };
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
        return db_error(&e);
    }
    let window = match days {
        Some(d) => format!("for {d} day(s)"),
        None => "until unmuted".to_string(),
    };
    format!("🔕 Muted {site_name} / {param_name} {window}.")
}

pub async fn unmute(db: &DatabaseConnection, args: &str) -> String {
    let toks: Vec<&str> = args.split_whitespace().collect();
    if toks.len() != 2 {
        return "Usage: /unmute <site> <param>".to_string();
    }
    let (site_id, site_name) = match resolve_site(db, &AccessScope::Unrestricted, toks[0]).await {
        Ok(SiteMatch::One(id, name)) => (id, name),
        Ok(SiteMatch::NotFound) => return format!("No site matches \"{}\".", toks[0]),
        Ok(SiteMatch::Ambiguous(names)) => return ambiguous_reply("sites", &names),
        Err(e) => return db_error(&e),
    };
    let (param_id, param_name) = match resolve_parameter(db, toks[1]).await {
        Ok(Ok(p)) => p,
        Ok(Err(msg)) => return msg,
        Err(e) => return db_error(&e),
    };
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
pub async fn muted(db: &DatabaseConnection) -> String {
    let mutes = match super::mutes_model::in_force_all(db).await {
        Ok(m) => m,
        Err(e) => return db_error(&e),
    };
    if mutes.is_empty() {
        return "No active mutes.".to_string();
    }

    let (site_names, param_names) = match mute_slot_names(db, &mutes).await {
        Ok(names) => names,
        Err(e) => return db_error(&e),
    };

    let unknown = "(unknown)".to_string();
    let mut lines: Vec<String> = mutes
        .iter()
        .map(|m| {
            let site = site_names.get(&m.site_id).unwrap_or(&unknown);
            let param = param_names.get(&m.parameter_id).unwrap_or(&unknown);
            let until = m.expires_at.map_or("permanent".to_string(), |e| {
                e.format("until %Y-%m-%d %H:%M UTC").to_string()
            });
            format!("{site} / {param} ({until})")
        })
        .collect();
    lines.sort();
    format!("Active mutes:\n{}", lines.join("\n"))
}

pub async fn start(
    db: &DatabaseConnection,
    chat_id: i64,
    _username: Option<&str>,
    code: &str,
) -> String {
    if code.is_empty() {
        return "Send /start <code> with the link code from your account settings.".to_string();
    }
    let res = db
        .query_one(Statement::from_sql_and_values(
            PG,
            "UPDATE telegram_identities \
             SET telegram_chat_id = $1, link_code = NULL, \
                 link_code_expires_at = NULL, is_active = TRUE, last_verified_at = NOW(), \
                 updated_at = NOW() \
             WHERE link_code = $2 AND link_code_expires_at > NOW() AND telegram_chat_id IS NULL \
             RETURNING id",
            [chat_id.into(), code.into()],
        ))
        .await;
    match res {
        Ok(Some(_)) => {
            "✅ Linked. You'll receive alerts and can use commands, try /help.".to_string()
        }
        Ok(None) => "Invalid or expired code.".to_string(),
        // Most likely the chat is already linked to another identity (unique constraint).
        Err(_) => "This chat is already linked, or the code is invalid.".to_string(),
    }
}

/// `/grab <site> <param> <value> [more values…]`, submit a grab sample (and its replicates) from
/// the field. Reuses the full grab-sample insert path (stream creation, sample aggregation, alarm
/// reconciliation). When `TELEGRAM_GRAB_FLAG_FOR_REVIEW` is set, the readings are flagged on insert
/// so they're held out of aggregates until a curator reviews them.
pub async fn grab(
    state: &AppState,
    scope: &AccessScope,
    args: &str,
    username: Option<&str>,
    chat_id: i64,
) -> String {
    let toks: Vec<&str> = args.split_whitespace().collect();
    if toks.len() < 3 {
        return "Usage: /grab <site> <param> <value> [more values…]".to_string();
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
    let created_by = username.map_or_else(
        || format!("telegram:{chat_id}"),
        |u| format!("telegram:{u}"),
    );
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
                flag_for_review(&state.db, site_id, param_id, now).await;
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

async fn flag_for_review(
    db: &DatabaseConnection,
    site_id: Uuid,
    param_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
) {
    let res = db
        .execute(Statement::from_sql_and_values(
            PG,
            "UPDATE readings SET is_flagged = TRUE, \
                 flag_reason = 'field submission – pending review' \
             WHERE site_id = $1 AND parameter_id = $2 AND time = $3",
            [site_id.into(), param_id.into(), time.into()],
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
