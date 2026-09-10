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

#[cfg(test)]
#[path = "tests/served.rs"]
mod tests;
