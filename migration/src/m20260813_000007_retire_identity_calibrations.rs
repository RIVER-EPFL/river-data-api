use sea_orm_migration::prelude::*;

/// Retires the calibrations the API used to mint for itself.
///
/// Until now several code paths created a slope-1.0/intercept-0.0 row so that every reading was
/// covered by some curve, and the writers materialised `raw_value` into `calibrated_value` to match.
/// Absence of a curve is now an ordinary state: an uncorrected reading carries no
/// `calibration_id` and a NULL `calibrated_value`, and a gap in a calibration timeline is legal.
/// The rows nobody performed are the last thing left asserting the old model, so they go.
///
/// They are identified by their provenance markers together, `performed_by = 'system'` AND
/// `notes = 'Identity calibration (auto-created)'`, the exact pair the deleted minter wrote. Never
/// by coefficients: a slope of 1.0 and an intercept of 0.0 entered by an operator is a real
/// calibration with real provenance (`performed_by`, `notes`, `name`, `r_squared`) and stays.
/// Matching is on the full `notes` string, not a `LIKE`, so a row that merely mentions the word is
/// untouched.
///
/// No continuous aggregate needs refreshing after this, which is a verified fact rather than an
/// assumption. The four aggregates `readings_hourly`/`_daily`/`_weekly`/`_monthly` are defined by
/// `m20260713_000002_aggregates_include_derived` (the newest migration to create them, superseding
/// `m20260325_000001_init`, `m20260508_000001`, `m20260603_000007` and `m20260711_000002`); every
/// measure is an aggregate over `COALESCE(calibrated_value, raw_value)` and no definition mentions
/// `calibration_id` at all. The `calibrated_value` this migration clears is `1.0 * raw_value + 0.0`,
/// which in IEEE 754 double precision is bit-identical to `raw_value`, so `COALESCE` serves the
/// identical number afterwards. On dev this holds exactly: of the 704,566 readings referencing an
/// auto-minted row, `calibrated_value IS DISTINCT FROM raw_value` matched none and
/// `calibrated_value IS NULL` matched none.
///
/// Where a reading also names a standard curve, its stored value is that curve applied on top of
/// the base and is *not* the raw value. Those keep their `calibrated_value`; only the reference to
/// the retired base is cleared.
///
/// This is not reversible and `down` says so rather than pretending. The deleted rows' ids and
/// provenance are gone, and the readings that pointed at them cannot be repointed at anything, so
/// any `down` would fabricate calibrations rather than restore them. An operator who wants a way
/// back must dump `sensor_calibrations` before applying this.
#[derive(DeriveMigrationName)]
pub struct Migration;

const AUTO_MINTED: &str =
    "performed_by = 'system' AND notes = 'Identity calibration (auto-created)'";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // `main.rs` issues a session-level `SET statement_timeout = '30s'` against the pool before
        // migrating; that lands on whichever connection the pool handed it, which may be the one
        // this migration draws. A ceiling sized for request handling would abort a bulk statement
        // that runs for minutes on a full deployment, and since the whole batch of pending
        // migrations shares one transaction, the abort would roll back every one of them. Raise the
        // ceiling for the transaction only: `SET LOCAL` reverts at commit and never touches the
        // pooled connection's session value. It does hold for any migration that follows in the
        // same batch, which only ever widens their ceiling. Explicit and generous rather than 0, so
        // a pathological run still fails instead of hanging startup under the migration advisory
        // lock.
        db.execute_unprepared("SET LOCAL statement_timeout = '30min';")
            .await?;

        // The readings UPDATE reaches compressed chunks, and TimescaleDB caps how many tuples one
        // transaction may decompress (default 100k). Same lift as `common::bulk_write`. `SET LOCAL`
        // only holds inside a transaction, which is where the migration batch already runs; do not
        // open an inner one.
        db.execute_unprepared(
            "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0;",
        )
        .await?;

        // One statement rather than two, so the compressed chunks are decompressed once.
        db.execute_unprepared(&format!(
            "UPDATE readings SET \
                 calibration_id = NULL, \
                 calibrated_value = CASE WHEN standard_curve_id IS NULL THEN NULL ELSE calibrated_value END \
             WHERE calibration_id IN (SELECT id FROM sensor_calibrations WHERE {AUTO_MINTED});"
        ))
        .await?;

        // Nothing references them now: `readings` is the only table with a foreign key to
        // `sensor_calibrations` (`m20260325_000001_init`), and `csv_import_staging.calibration_id`
        // is a plain column on transient rows.
        db.execute_unprepared(&format!(
            "DELETE FROM sensor_calibrations WHERE {AUTO_MINTED};"
        ))
        .await?;

        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Deliberately a no-op, see the type's documentation: the retired rows and the references
        // to them are gone, so there is nothing to restore and re-minting would invent data.
        Ok(())
    }
}
