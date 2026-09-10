//! What the two serving arms are, in one place, for every surface that answers "what does this
//! slot serve at this instant".
//!
//! Continuous and derived rows are written at `replicate_index = 0` by every continuous writer, so
//! the plain equality is exact and keeps ordered-append scans and hash aggregation. A spot instant
//! is a replicate group served at the trigger-maintained sample mean over its unflagged
//! replicates, with the lowest unflagged replicate's own value as the fallback when no sample row
//! exists (unpaired stream, or not yet materialised). A fully flagged group serves nothing;
//! flagging one replicate moves the served value rather than removing the instant.
//!
//! Every predicate here is written against the alias `r` on `readings`.
//!
//! Three neighbouring queries deliberately do not serve through these, and are not drift:
//! the instrument diagnostic view (`sensors/readings.rs`) keeps flagged points visible because
//! that is what it is for; the rollup population filter (`common/aggregates.rs`) mirrors the
//! continuous aggregates' own definition, which lives in the migration and can only move with
//! one; and the staleness probes (`notifications/triggers.rs`) ask when a slot last received
//! anything, not what it serves.

use sea_orm::Order;
use sea_orm::sea_query::{Alias, Condition, Expr, ExprTrait, Func};

use crate::routes::private::readings::models as readings;

/// Continuous and derived rows, by cadence alone. A surface that serves values adds its own
/// curation rule on top: [`SERVED_CONTINUOUS`] is that rule for a chart or an export, while alarm
/// evaluation deliberately keeps flagged continuous readings alerting and so uses this.
pub const CONTINUOUS_ROWS: &str =
    "r.measurement_type IS DISTINCT FROM 'spot' AND r.replicate_index = 0";

/// Spot replicates, by cadence alone, withdrawn rows excluded: a value the source has taken back
/// is not a measurement any surface holds an opinion about.
pub const SPOT_ROWS: &str = "r.measurement_type = 'spot' AND r.withdrawn_at IS NULL";

/// What a curated surface leaves out of both arms: a flagged reading, and an intern's entry that
/// no manager has verified.
pub const NOT_CURATED_OUT: &str = "r.is_flagged IS NOT TRUE AND r.unverified IS NOT TRUE";

/// Continuous and derived rows a curated surface serves.
pub const SERVED_CONTINUOUS: &str = "r.measurement_type IS DISTINCT FROM 'spot' \
     AND r.replicate_index = 0 AND r.is_flagged IS NOT TRUE AND r.unverified IS NOT TRUE";

/// Spot replicates a curated surface serves. One instant yields several of these rows, one per
/// replicate, collapsed to the served value by [`SPOT_INSTANT_KEY`] and [`SPOT_INSTANT_ORDER`].
pub const SERVED_SPOT: &str = "r.measurement_type = 'spot' AND r.is_flagged IS NOT TRUE \
     AND r.withdrawn_at IS NULL AND r.unverified IS NOT TRUE";

/// `DISTINCT ON` key collapsing a spot replicate group to its instant. The instant is the slot's,
/// not the stream's: a `(site, parameter, time)` group is one sample whatever number of streams
/// fed it, which is what the materialiser and the samples trigger already say. Keying it per
/// stream returned the same instant twice.
pub const SPOT_INSTANT_KEY: &str = "r.parameter_id, r.time";

/// The ordering [`SPOT_INSTANT_KEY`] picks its surviving row by: a live replicate over a
/// withdrawn one, an unflagged replicate over a flagged one, then the lowest replicate index, with
/// `stream_id` last so the survivor is stable across requests when two streams feed the instant.
pub const SPOT_INSTANT_ORDER: &str = "r.parameter_id, r.time, (r.withdrawn_at IS NOT NULL), \
     (r.is_flagged IS TRUE), r.replicate_index, r.stream_id";

/// The served value of a spot replicate group: its sample mean, else the surviving replicate's
/// own value.
pub const SPOT_VALUE: &str = "COALESCE(smp.mean, r.calibrated_value, r.raw_value)";

/// The served value of a continuous or derived row.
pub const CONTINUOUS_VALUE: &str = "COALESCE(r.calibrated_value, r.raw_value)";

// --- The same rules as expressions ---
//
// A fragment shared between call sites composes only as an expression; the `&str` forms above are
// what a caller still spelling its query as text pastes, and go with the last of them.

/// The alias every rule below is written against: the readings row.
#[must_use]
pub fn r() -> Alias {
    Alias::new("r")
}

fn col(column: readings::Column) -> Expr {
    Expr::col((r(), column))
}

/// [`CONTINUOUS_ROWS`] as a condition.
#[must_use]
pub fn continuous_rows() -> Condition {
    Condition::all()
        .add(Expr::cust("r.measurement_type IS DISTINCT FROM 'spot'"))
        .add(col(readings::Column::ReplicateIndex).eq(0))
}

/// [`SPOT_ROWS`] as a condition.
#[must_use]
pub fn spot_rows() -> Condition {
    Condition::all()
        .add(col(readings::Column::MeasurementType).eq("spot"))
        .add(col(readings::Column::WithdrawnAt).is_null())
}

/// [`NOT_CURATED_OUT`] as a condition.
#[must_use]
pub fn not_curated_out() -> Condition {
    Condition::all()
        .add(Expr::cust("r.is_flagged IS NOT TRUE"))
        // `NOT <col>` rather than `IS NOT TRUE`: the column is `NOT NULL`, so the two agree, and
        // the builder writes `IS NOT` with a bind, which Postgres will not parse.
        .add(col(readings::Column::Unverified).not())
}

/// [`SERVED_CONTINUOUS`] as a condition.
#[must_use]
pub fn served_continuous() -> Condition {
    continuous_rows().add(not_curated_out())
}

/// [`SERVED_SPOT`] as a condition.
#[must_use]
pub fn served_spot() -> Condition {
    spot_rows().add(not_curated_out())
}

/// [`SERVED_SPOT`] against a caller's own alias for `readings`, for a query that does not use `r`.
#[must_use]
pub fn served_spot_at(alias: &Alias) -> Condition {
    Condition::all()
        .add(Expr::col((alias.clone(), readings::Column::MeasurementType)).eq("spot"))
        .add(
            Expr::col((alias.clone(), readings::Column::IsFlagged))
                .ne(true)
                .or(Expr::col((alias.clone(), readings::Column::IsFlagged)).is_null()),
        )
        .add(Expr::col((alias.clone(), readings::Column::WithdrawnAt)).is_null())
}

/// [`SPOT_INSTANT_KEY`] as the `DISTINCT ON` columns.
#[must_use]
pub fn spot_instant_key() -> [(Alias, readings::Column); 2] {
    [
        (r(), readings::Column::ParameterId),
        (r(), readings::Column::Time),
    ]
}

/// [`SPOT_INSTANT_ORDER`] as the ordering that picks the surviving row.
#[must_use]
pub fn spot_instant_order() -> Vec<(Expr, Order)> {
    vec![
        (col(readings::Column::ParameterId), Order::Asc),
        (col(readings::Column::Time), Order::Asc),
        (Expr::cust("(r.withdrawn_at IS NOT NULL)"), Order::Asc),
        (Expr::cust("(r.is_flagged IS TRUE)"), Order::Asc),
        (col(readings::Column::ReplicateIndex), Order::Asc),
        (col(readings::Column::StreamId), Order::Asc),
    ]
}

/// [`SPOT_VALUE`] as an expression: the group's sample mean, else the surviving replicate's own.
#[must_use]
pub fn spot_value() -> Expr {
    Expr::cust("COALESCE(smp.mean, r.calibrated_value, r.raw_value)")
}

/// [`CONTINUOUS_VALUE`] as an expression.
#[must_use]
pub fn continuous_value() -> Expr {
    continuous_value_of(r())
}

/// [`CONTINUOUS_VALUE`] against a caller's own alias for `readings`.
#[must_use]
pub fn continuous_value_of(alias: Alias) -> Expr {
    Expr::expr(Func::coalesce([
        Expr::col((alias.clone(), readings::Column::CalibratedValue)),
        Expr::col((alias, readings::Column::RawValue)),
    ]))
}

#[cfg(test)]
#[path = "tests/served.rs"]
mod tests;
