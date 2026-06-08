//! Shared alarm-threshold resolution and severity logic.
//!
//! The breach definition (which value counts as warning vs alarm) and the 3-priority threshold
//! fallback live here so the live evaluation (`views.rs`, `sweeper.rs`) and the historical episode
//! rebuild (`episodes.rs`) can't drift apart.

use sea_orm::{ConnectionTrait, DatabaseConnection, FromQueryResult, Statement};
use uuid::Uuid;

/// The four numeric bounds that define a breach for one (parameter, site) slot.
#[derive(Debug, Clone, Copy, FromQueryResult)]
pub struct ResolvedThreshold {
    pub warning_min: Option<f64>,
    pub warning_max: Option<f64>,
    pub alarm_min: Option<f64>,
    pub alarm_max: Option<f64>,
}

impl ResolvedThreshold {
    /// A threshold with every bound NULL never fires — this is the "Disabled" state written by the
    /// UI (a null-valued row at priority 1 that blocks the parameter-default fallback).
    pub fn is_disabled(&self) -> bool {
        self.warning_min.is_none()
            && self.warning_max.is_none()
            && self.alarm_min.is_none()
            && self.alarm_max.is_none()
    }
}

/// SQL `CASE` mapping a value to a severity (`2`=alarm, `1`=warning, `0`=ok). Callers pass the value
/// expression and the four bound expressions — column refs for the live queries (`t.alarm_min`,
/// `rt.alarm_min`), bind-param refs for the episode query (`$7::double precision`) — so the severity
/// ladder is defined in exactly one place. Result is a bare integer; cast to `smallint` at the call
/// site if needed.
pub fn severity_case(val: &str, wmin: &str, wmax: &str, amin: &str, amax: &str) -> String {
    format!(
        "CASE \
            WHEN ({amin} IS NOT NULL AND {val} < {amin}) OR ({amax} IS NOT NULL AND {val} > {amax}) THEN 2 \
            WHEN ({wmin} IS NOT NULL AND {val} < {wmin}) OR ({wmax} IS NOT NULL AND {val} > {wmax}) THEN 1 \
            ELSE 0 \
        END"
    )
}

/// Boolean SQL predicate true when a value breaches at or above `min_severity` (1 includes warnings,
/// 2 restricts to alarms). Same bound-expression convention as [`severity_case`].
pub fn violation_condition(
    val: &str,
    wmin: &str,
    wmax: &str,
    amin: &str,
    amax: &str,
    min_severity: i16,
) -> String {
    let alarm = format!("({amin} IS NOT NULL AND {val} < {amin}) OR ({amax} IS NOT NULL AND {val} > {amax})");
    if min_severity >= 2 {
        format!("({alarm})")
    } else {
        format!(
            "({alarm} OR ({wmin} IS NOT NULL AND {val} < {wmin}) OR ({wmax} IS NOT NULL AND {val} > {wmax}))"
        )
    }
}

/// Resolve the effective threshold for one (site, parameter) slot via the 3-priority fallback:
/// site-specific row → global row (`site_id IS NULL`) → parameter defaults. Returns `None` when the
/// parameter has no threshold anywhere (it can never alarm). An all-NULL row (the "Disabled" state)
/// resolves to `Some(..)` with every bound `None` and suppresses alarms — see
/// [`ResolvedThreshold::is_disabled`].
pub async fn resolve_threshold(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Result<Option<ResolvedThreshold>, sea_orm::DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"
            SELECT warning_min, warning_max, alarm_min, alarm_max
            FROM (
                SELECT t.warning_min, t.warning_max, t.alarm_min, t.alarm_max,
                       CASE WHEN t.site_id = $1 THEN 1 ELSE 2 END AS priority
                FROM alarm_thresholds t
                WHERE t.parameter_id = $2 AND (t.site_id = $1 OR t.site_id IS NULL)
                UNION ALL
                SELECT p.default_warning_min, p.default_warning_max,
                       p.default_alarm_min, p.default_alarm_max, 3
                FROM parameters p
                WHERE p.id = $2
                  AND (p.default_warning_min IS NOT NULL OR p.default_warning_max IS NOT NULL
                       OR p.default_alarm_min IS NOT NULL OR p.default_alarm_max IS NOT NULL)
            ) s
            ORDER BY priority
            LIMIT 1
            ",
            [site_id.into(), parameter_id.into()],
        ))
        .await?;

    row.map(|r| ResolvedThreshold::from_query_result(&r, ""))
        .transpose()
}
