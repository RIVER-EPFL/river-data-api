use sea_orm_migration::prelude::*;

/// Move every deployment that pairing created to the start of the history it should cover.
///
/// `find_or_create_deployment` used to open at the pairing instant, so a stream paired months after
/// it started measuring left every earlier reading attributed to the site with no deployment and no
/// window-resolved calibration. The rows are recognisable by the note pairing writes, which nothing
/// else writes.
///
/// The new start is the slot's earliest reading, reached both through the stream and through the
/// readings' own attribution, clamped forward to the end of the last deployment that covered the
/// slot before it: that instrument owns the history it covered, and the clamp is what keeps the
/// slot's exclusion constraint satisfied. A row with no readings to cover is left alone.
///
/// This moves deployments only. The readings under them are re-derived by
/// `POST /api/actions/reprocess_all`, which an operator runs once after the rollout in each
/// environment.
#[derive(DeriveMigrationName)]
pub struct Migration;

/// The marker `sensors::operations::find_or_create_deployment` writes and nothing else does;
/// `deployment_type` is shared with hand-made rows, so the note is the only marker.
#[must_use]
pub fn backdate_auto_deployments() -> String {
    r"
    WITH auto AS (
        SELECT d.id,
               d.deployed_from,
               LEAST(
                   (SELECT MIN(r.time) FROM readings r
                     WHERE r.sensor_id = d.sensor_id
                       AND r.site_id = d.site_id
                       AND r.parameter_id = d.parameter_id),
                   (SELECT MIN(r.time) FROM readings r
                      JOIN data_streams ds ON ds.id = r.stream_id
                      JOIN site_parameters sp ON sp.id = ds.site_parameter_id
                     WHERE ds.sensor_id = d.sensor_id
                       AND sp.site_id = d.site_id
                       AND sp.parameter_id = d.parameter_id)
               ) AS first_reading,
               (SELECT MAX(o.deployed_until) FROM sensor_deployments o
                 WHERE o.site_id = d.site_id
                   AND o.parameter_id = d.parameter_id
                   AND o.id <> d.id
                   AND o.deployed_until IS NOT NULL
                   AND o.deployed_until <= d.deployed_from) AS prior_end
          FROM sensor_deployments d
         WHERE d.notes = 'Auto-created during stream pairing'
    )
    UPDATE sensor_deployments d
       SET deployed_from = GREATEST(a.first_reading, COALESCE(a.prior_end, a.first_reading))
      FROM auto a
     WHERE d.id = a.id
       AND a.first_reading IS NOT NULL
       AND GREATEST(a.first_reading, COALESCE(a.prior_end, a.first_reading)) < a.deployed_from;
    "
    .to_string()
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(&backdate_auto_deployments())
            .await?;
        Ok(())
    }

    /// A start date the repair moved cannot be recovered: the value it replaced was the pairing
    /// instant, which nothing records.
    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "backdated deployment starts cannot be restored: the pairing instant is not recorded"
                .to_string(),
        ))
    }
}
