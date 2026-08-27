//! The typed declaration that one stream is fed by several portal columns as replicates of one
//! logical parameter. Registered by a sync service on `/streams/register`, persisted under
//! `data_streams.metadata["replicates"]`, and read back by the UI and the reconciliation job.
//!
//! First-class means validated and refused here, never inferred from column-name shape: the
//! portals' naming carries five inconsistent suffix dialects and several traps (`S275_295` is a
//! wavelength range, `WTW_pH_1` a legacy marker), so membership only ever comes from the source's
//! own calculation registry, carried in this spec.

use sea_orm::{ConnectionTrait, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::{AppError, AppResult};

/// The metadata key the spec is stored under.
pub const METADATA_KEY: &str = "replicates";

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReplicateSpec {
    /// Source columns in order; position is the replicate_index the member's readings carry.
    pub source_columns: Vec<String>,
    /// The portal's precomputed mean column. Audited at sync time, never a stream.
    #[serde(default)]
    pub portal_mean_column: Option<String>,
    /// The portal's precomputed standard-deviation column. Audited, never a stream.
    #[serde(default)]
    pub portal_sd_column: Option<String>,
    /// The portal column holding the per-row standard-curve reference, when the family's values
    /// are corrected through one (e.g. `doc_std_curve_id`).
    #[serde(default)]
    pub curve_ref_column: Option<String>,
    /// How the portal derives its mean from the members, as declared by its calculation registry
    /// (e.g. `calcMean`, `calcDOCavg`). Recorded for provenance and for the audit's semantics.
    #[serde(default)]
    pub calc: Option<String>,
}

impl ReplicateSpec {
    /// Refuse a spec that cannot describe a replicate family. Two or more members, no duplicates,
    /// and the stream must be classified spot: sample formation is spot-only, so a continuous
    /// stream declaring replicates would silently never form the samples the spec promises.
    pub fn validate(&self, stream_measurement_type: Option<&str>) -> AppResult<()> {
        if self.source_columns.len() < 2 {
            return Err(AppError::BadRequest(
                "a replicate spec needs at least two source columns; a single-column stream \
                 carries no replicates to declare"
                    .to_string(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for col in &self.source_columns {
            if col.trim().is_empty() {
                return Err(AppError::BadRequest(
                    "replicate source columns cannot be empty".to_string(),
                ));
            }
            if !seen.insert(col.as_str()) {
                return Err(AppError::BadRequest(format!(
                    "replicate source column '{col}' is listed twice"
                )));
            }
        }
        if stream_measurement_type != Some(crate::routes::private::readings::sample_groups::SPOT) {
            return Err(AppError::BadRequest(
                "a stream declaring replicates must be classified 'spot': samples only form from \
                 spot readings"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Store the spec under [`METADATA_KEY`] in a stream's metadata object.
    pub fn embed(&self, metadata: &mut serde_json::Value) -> AppResult<()> {
        let spec = serde_json::to_value(self)
            .map_err(|e| AppError::Internal(format!("replicate spec serialisation: {e}")))?;
        match metadata {
            serde_json::Value::Object(map) => {
                map.insert(METADATA_KEY.to_string(), spec);
                Ok(())
            }
            serde_json::Value::Null => {
                *metadata = serde_json::json!({ METADATA_KEY: spec });
                Ok(())
            }
            _ => Err(AppError::BadRequest(
                "stream metadata must be an object to carry a replicate spec".to_string(),
            )),
        }
    }

    /// Parse the spec out of a stream's metadata, if one was registered.
    #[must_use]
    pub fn from_metadata(metadata: &serde_json::Value) -> Option<Self> {
        serde_json::from_value(metadata.get(METADATA_KEY)?.clone()).ok()
    }
}

/// The `source_key`s of the replicate families in a stream selection, matching the selection
/// `/streams/retag` updates.
pub async fn family_keys_in_streams<C: ConnectionTrait>(
    db: &C,
    stream_ids: &[Uuid],
    source_system: Option<&str>,
) -> AppResult<Vec<String>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT source_key FROM data_streams \
             WHERE (id = ANY($1) OR ($2::text IS NOT NULL AND source_system = $2)) \
               AND metadata -> $3 IS NOT NULL \
             ORDER BY source_key",
            [
                stream_ids.to_vec().into(),
                source_system.map(ToString::to_string).into(),
                METADATA_KEY.into(),
            ],
        ))
        .await?;
    Ok(rows
        .iter()
        .filter_map(|r| r.try_get::<String>("", "source_key").ok())
        .collect())
}

/// The `source_key`s of the replicate families owned by these sensors.
pub async fn family_keys_for_sensors<C: ConnectionTrait>(
    db: &C,
    sensor_ids: &[Uuid],
) -> AppResult<Vec<String>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT source_key FROM data_streams \
             WHERE sensor_id = ANY($1) AND metadata -> $2 IS NOT NULL \
             ORDER BY source_key",
            [sensor_ids.to_vec().into(), METADATA_KEY.into()],
        ))
        .await?;
    Ok(rows
        .iter()
        .filter_map(|r| r.try_get::<String>("", "source_key").ok())
        .collect())
}

/// A replicate family stays classified spot for the same reason one may not be registered any
/// other way: the continuous aggregates roll up only non-spot rows at `replicate_index = 0`, so a
/// family outside spot would have every replicate but one dropped from every rollup.
pub fn refuse_family_retag(keys: &[String], target: &str) -> AppResult<()> {
    if keys.is_empty() {
        return Ok(());
    }
    Err(AppError::BadRequest(format!(
        "these streams declare replicate families and must stay classified 'spot', not \
         '{target}': {}. The continuous aggregates roll up only non-spot rows at replicate \
         index 0, so a family outside 'spot' loses every replicate but one from every rollup",
        keys.join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(columns: &[&str]) -> ReplicateSpec {
        ReplicateSpec {
            source_columns: columns.iter().map(ToString::to_string).collect(),
            portal_mean_column: Some("DOC_avg_ppb".to_string()),
            portal_sd_column: Some("DOC_sd_ppb".to_string()),
            curve_ref_column: Some("doc_std_curve_id".to_string()),
            calc: Some("calcDOCavg".to_string()),
        }
    }

    #[test]
    fn a_valid_spec_roundtrips_through_metadata() {
        let s = spec(&["DOC_rep_1", "DOC_rep_2", "DOC_rep_3"]);
        s.validate(Some("spot")).unwrap();
        let mut metadata = serde_json::json!({"hierarchy": {"site": "DGT"}});
        s.embed(&mut metadata).unwrap();
        let parsed = ReplicateSpec::from_metadata(&metadata).unwrap();
        assert_eq!(parsed.source_columns, s.source_columns);
        assert_eq!(parsed.portal_mean_column.as_deref(), Some("DOC_avg_ppb"));
        assert_eq!(metadata["hierarchy"]["site"], "DGT");
    }

    #[test]
    fn a_single_member_is_refused() {
        assert!(spec(&["DOC_rep_1"]).validate(Some("spot")).is_err());
    }

    #[test]
    fn duplicate_members_are_refused() {
        assert!(
            spec(&["DOC_rep_1", "DOC_rep_1"])
                .validate(Some("spot"))
                .is_err()
        );
    }

    #[test]
    fn a_non_spot_stream_cannot_declare_replicates() {
        assert!(
            spec(&["DOC_rep_1", "DOC_rep_2"])
                .validate(Some("continuous"))
                .is_err()
        );
        assert!(spec(&["DOC_rep_1", "DOC_rep_2"]).validate(None).is_err());
    }
}
