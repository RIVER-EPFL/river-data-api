//! Who holds a `(site, parameter)` deployment slot over a time window.
//!
//! `excl_deployment_site_param_slot` is the atomic backstop: one sensor per `(site, parameter)` at
//! any instant, enforced by an `EXCLUDE USING gist` constraint with no sensor term. Every path that
//! writes a deployment asks [`find_occupant`] first so the operator gets an actionable answer naming
//! the blocking row instead of a raw constraint violation, and, on the CRUD create path, so a
//! refused request has not already moved something.
//!
//! The constraint has no sensor term, so a sensor can collide with *itself*: a second historical
//! deployment of the same instrument at the same slot over an overlapping window is as illegal as
//! another instrument's. A check filtered on `sensor_id <> incoming` misses exactly that case, which
//! is why the exclusion here is by pending recall rather than by sensor.

use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DbErr, Statement};
use uuid::Uuid;

/// The window a caller is about to claim, and what the write will do to the slot before claiming it.
#[derive(Debug, Clone, Copy)]
pub struct SlotRequest {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub deployed_from: DateTime<Utc>,
    pub deployed_until: Option<DateTime<Utc>>,
    /// A deployment row being edited, which cannot conflict with itself.
    pub exclude_deployment: Option<Uuid>,
    /// The sensor whose still-open deployments this write closes at `deployed_from`. Those rows end
    /// where the new window starts, so they cannot overlap it and must not be reported as blocking.
    pub recalled_sensor: Option<Uuid>,
}

/// A deployment already covering part of the requested window.
#[derive(Debug, Clone, Copy)]
pub struct SlotOccupant {
    pub deployment_id: Uuid,
    pub sensor_id: Uuid,
    pub deployed_from: DateTime<Utc>,
    pub deployed_until: Option<DateTime<Utc>>,
}

/// The earliest deployment blocking `req`, or `None` when the slot is free over the window.
pub async fn find_occupant<C: ConnectionTrait>(
    db: &C,
    req: &SlotRequest,
) -> Result<Option<SlotOccupant>, DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT d.id, d.sensor_id, d.deployed_from, d.deployed_until
              FROM sensor_deployments d
              WHERE d.site_id = $1
                AND d.parameter_id = $2
                AND ($3::uuid IS NULL OR d.id <> $3::uuid)
                AND NOT ($4::uuid IS NOT NULL
                         AND d.sensor_id = $4::uuid
                         AND d.deployed_until IS NULL
                         AND d.deployed_from <= $5)
                AND tstzrange(d.deployed_from, COALESCE(d.deployed_until, 'infinity'::timestamptz), '[)')
                    && tstzrange($5, COALESCE($6, 'infinity'::timestamptz), '[)')
              ORDER BY d.deployed_from
              LIMIT 1",
            [
                req.site_id.into(),
                req.parameter_id.into(),
                req.exclude_deployment.into(),
                req.recalled_sensor.into(),
                req.deployed_from.into(),
                req.deployed_until.into(),
            ],
        ))
        .await?;

    let Some(row) = row else {
        return Ok(None);
    };
    let deployed_from: DateTime<chrono::FixedOffset> = row.try_get("", "deployed_from")?;
    let deployed_until: Option<DateTime<chrono::FixedOffset>> =
        row.try_get("", "deployed_until")?;
    Ok(Some(SlotOccupant {
        deployment_id: row.try_get("", "id")?,
        sensor_id: row.try_get("", "sensor_id")?,
        deployed_from: deployed_from.with_timezone(&Utc),
        deployed_until: deployed_until.map(|t| t.with_timezone(&Utc)),
    }))
}

/// What to tell the operator, naming the blocking row. `remedy` completes "Recall it first, then
/// …", ie. "deploy" or "move this deployment".
#[must_use]
pub fn conflict_message(occupant: &SlotOccupant, incoming_sensor: Uuid, remedy: &str) -> String {
    let until = occupant
        .deployed_until
        .map_or_else(|| "open".to_string(), |t| t.to_rfc3339());
    if occupant.sensor_id == incoming_sensor {
        format!(
            "This instrument already holds this site and parameter over an overlapping period \
             (deployment {} from {} to {}). Edit that deployment instead.",
            occupant.deployment_id,
            occupant.deployed_from.to_rfc3339(),
            until
        )
    } else {
        format!(
            "Another sensor is already deployed to this site for this parameter over an \
             overlapping period. Recall it first, then {remedy}. (sensor {}, deployment {}, {} to {})",
            occupant.sensor_id,
            occupant.deployment_id,
            occupant.deployed_from.to_rfc3339(),
            until
        )
    }
}

/// Close the sensor's open deployments on this channel at `at`, and report how many closed.
///
/// The one recall. Scoped to the SAME parameter (channel): a multi-channel instrument holds one open
/// deployment per parameter, so deploying its temperature channel must not close its still-live
/// conductivity channel. A same-parameter move across sites still closes the old site's row.
///
/// `deployed_from <= at` keeps every row a valid range: a backdated deploy must not close a
/// later-starting deployment at an instant before it began. Such a row overlaps the incoming window
/// instead, and [`find_occupant`] refuses the request before this runs.
pub async fn recall_open_deployments<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
    parameter_id: Uuid,
    at: DateTime<Utc>,
    except: Option<Uuid>,
) -> Result<u64, DbErr> {
    let result = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE sensor_deployments
              SET deployed_until = $1
              WHERE sensor_id = $2
                AND parameter_id = $3
                AND deployed_until IS NULL
                AND deployed_from <= $1
                AND ($4::uuid IS NULL OR id <> $4::uuid)",
            [
                at.into(),
                sensor_id.into(),
                parameter_id.into(),
                except.into(),
            ],
        ))
        .await?;
    Ok(result.rows_affected())
}

/// Carry an adjacent predecessor's end date with a deployment whose start moves forward, and report
/// how many rows followed.
///
/// `calibrations::service::recompute_deployed_until` only ever shortens a window, because deployment
/// coverage is gap-preserving: an instrument legitimately sits in the lab between campaigns. A move date corrected forward would therefore leave `[old_from, new_from)` owned by
/// nobody, and the recall pass drops its readings out of every site instead of handing them back to
/// where the instrument actually was.
///
/// Adjacency is the whole test. Only a predecessor that ended exactly at `old_from` follows: one
/// that ended earlier had a real gap after it and keeps it, and a deletion, which leaves no
/// successor to be adjacent to, stretches nothing.
///
/// Called before the moved row itself is written, so a predecessor sharing the moved row's own
/// `(site, parameter)` slot is left alone: the vacated window is still occupied at this point and
/// extending into it would raise the exclusion constraint.
pub async fn follow_forward_move<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
    parameter_id: Uuid,
    moved_deployment: Uuid,
    old_from: DateTime<Utc>,
    new_from: DateTime<Utc>,
) -> Result<u64, DbErr> {
    if new_from <= old_from {
        return Ok(0);
    }
    let result = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE sensor_deployments p
              SET deployed_until = $5
              WHERE p.sensor_id = $1
                AND p.parameter_id = $2
                AND p.id <> $3
                AND p.deployed_until = $4
                AND p.deployed_from < $5
                AND NOT EXISTS (
                    SELECT 1 FROM sensor_deployments o
                    WHERE o.site_id = p.site_id
                      AND o.parameter_id = p.parameter_id
                      AND o.id <> p.id
                      AND tstzrange(o.deployed_from, COALESCE(o.deployed_until, 'infinity'::timestamptz), '[)')
                          && tstzrange($4, $5, '[)')
                )",
            [
                sensor_id.into(),
                parameter_id.into(),
                moved_deployment.into(),
                old_from.into(),
                new_from.into(),
            ],
        ))
        .await?;
    Ok(result.rows_affected())
}

/// Whether a database error is the deployment slot exclusion violation, the backstop firing on a
/// window no pre-check saw (a concurrent double-deploy).
#[must_use]
pub fn is_slot_conflict(err: &DbErr) -> bool {
    let msg = err.to_string();
    msg.contains("excl_deployment_site_param_slot") || msg.contains("23P01")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn occupant(sensor: Uuid, until: Option<DateTime<Utc>>) -> SlotOccupant {
        SlotOccupant {
            deployment_id: Uuid::nil(),
            sensor_id: sensor,
            deployed_from: DateTime::from_timestamp(0, 0).expect("epoch"),
            deployed_until: until,
        }
    }

    #[test]
    fn a_self_collision_reads_as_an_edit_not_a_recall() {
        let sensor = Uuid::from_u128(1);
        let message = conflict_message(&occupant(sensor, None), sensor, "deploy");
        assert!(
            message.contains("Edit that deployment instead"),
            "recalling itself is not a remedy the operator can act on: {message}"
        );
    }

    #[test]
    fn another_instruments_collision_keeps_the_recall_wording() {
        let message = conflict_message(
            &occupant(Uuid::from_u128(1), None),
            Uuid::from_u128(2),
            "deploy",
        );
        assert!(
            message
                .starts_with("Another sensor is already deployed to this site for this parameter"),
            "{message}"
        );
        assert!(
            message.contains("Recall it first, then deploy"),
            "{message}"
        );
    }

    #[test]
    fn an_open_window_is_named_as_open() {
        let message = conflict_message(
            &occupant(Uuid::from_u128(1), None),
            Uuid::from_u128(2),
            "deploy",
        );
        assert!(message.contains("to open"), "{message}");
    }

    #[test]
    fn only_the_exclusion_constraint_reads_as_a_slot_conflict() {
        assert!(is_slot_conflict(&DbErr::Custom(
            "conflicting key value violates exclusion constraint \
             \"excl_deployment_site_param_slot\""
                .to_string()
        )));
        assert!(is_slot_conflict(&DbErr::Custom(
            "SQLSTATE 23P01".to_string()
        )));
        assert!(!is_slot_conflict(&DbErr::Custom(
            "duplicate key value violates unique constraint \"sensors_pkey\"".to_string()
        )));
    }
}
