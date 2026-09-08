use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};
use uuid::Uuid;

use super::model;
use crate::error::AppError;

/// The key a stream's declared decimal places are stored under in `data_streams.metadata`.
pub const DECIMAL_PLACES_KEY: &str = "decimal_places";

/// The decimal places the stream's source declared at registration, if any.
#[must_use]
pub fn declared_decimal_places(metadata: &serde_json::Value) -> Option<i16> {
    metadata
        .get(DECIMAL_PLACES_KEY)
        .and_then(serde_json::Value::as_i64)
        .and_then(|n| i16::try_from(n).ok())
}

/// Write a declaration onto a slot that has none. A slot's own declaration is an operator's and
/// is never overwritten. Returns whether the slot was written.
pub async fn declare_slot_decimal_places<C: sea_orm::ConnectionTrait>(
    db: &C,
    site_parameter_id: Uuid,
    decimal_places: Option<i16>,
) -> Result<bool, sea_orm::DbErr> {
    let Some(places) = decimal_places else {
        return Ok(false);
    };
    let written = db
        .execute_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE site_parameters SET decimal_places = $1, updated_at = NOW() \
             WHERE id = $2 AND decimal_places IS NULL",
            [places.into(), site_parameter_id.into()],
        ))
        .await?
        .rows_affected();
    Ok(written > 0)
}

/// Get or create an "api" stream for a given (site_id, parameter_id) pair.
///
/// Used by batch insert endpoints to assign a stream_id to API-submitted readings.
/// Upserts on (source_system="api", source_key="{site_id}:{parameter_id}").
/// The slot a (site, parameter) pair names, or `None` when the parameter is not assigned to the
/// site. It is the one place a reading's attribution comes from.
pub async fn site_parameter_of(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Result<Option<Uuid>, AppError> {
    use sea_orm::{ConnectionTrait, Statement};
    Ok(db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM site_parameters WHERE site_id = $1 AND parameter_id = $2 LIMIT 1",
            [site_id.into(), parameter_id.into()],
        ))
        .await?
        .map(|row| row.try_get::<Uuid>("", "id"))
        .transpose()?)
}

pub async fn get_or_create_api_stream(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Result<Uuid, AppError> {
    let source_key = format!("{site_id}:{parameter_id}");

    // Try to find existing
    if let Some(stream) = model::Entity::find()
        .filter(model::Column::SourceSystem.eq("api"))
        .filter(model::Column::SourceKey.eq(&source_key))
        .one(db)
        .await?
    {
        crate::routes::private::sensors::identity::ensure_channel_instrument(
            db,
            &stream,
            site_id,
            parameter_id,
            "API entry",
        )
        .await?;
        return Ok(stream.id);
    }

    // Create new
    let now = chrono::Utc::now();
    let id = Uuid::new_v4();
    let site_parameter_id = site_parameter_of(db, site_id, parameter_id).await?;
    let active_model = model::ActiveModel {
        id: Set(id),
        source_system: Set("api".to_string()),
        source_key: Set(source_key.clone()),
        source_name: Set(Some("API batch insert".to_string())),
        source_path: Set(None),
        metadata: Set(serde_json::json!({})),
        // Paired on creation: this channel exists to carry one slot's readings, and attribution is
        // read from the pairing rather than restated per row. A slot that has no `site_parameters`
        // row yet leaves the stream unpaired, like any other undiscovered channel.
        site_parameter_id: Set(site_parameter_id),
        paired_at: Set(site_parameter_id.map(|_| now.into())),
        sensor_id: Set(None),
        measurement_type: Set(None),
        is_active: Set(true),
        discovered_at: Set(now.into()),
        last_data_time: Set(None),
        last_window_digest: Set(None),
        pairing_plan_id: Set(None),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    };

    model::Entity::insert(active_model)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                model::Column::SourceSystem,
                model::Column::SourceKey,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(db)
        .await
        .map_err(AppError::Database)?;

    // Re-fetch in case of race condition (ON CONFLICT DO NOTHING returns no id)
    let stream = model::Entity::find()
        .filter(model::Column::SourceSystem.eq("api"))
        .filter(model::Column::SourceKey.eq(&source_key))
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to create API stream".to_string()))?;

    // The channel carries an instrument from the moment it exists, so nothing written through it
    // can land without one.
    crate::routes::private::sensors::identity::ensure_channel_instrument(
        db,
        &stream,
        site_id,
        parameter_id,
        "API entry",
    )
    .await?;

    Ok(stream.id)
}

/// Why this source's streams may not be paired yet, or `None` when they may.
///
/// NOMIS reports a plain date and a plain time with no zone column, unlike CNET and METALP, and
/// the connector reads them as UTC (`nomis/mod.rs`, `parse_nomis_datetime`). ADR 0004 records that
/// as an assumption to be confirmed before any NOMIS data is paired: if the columns are Valais
/// wall clock, every NOMIS grab lands one or two hours off, attaches to the wrong collection event
/// and is compared against a sensor window shifted by the same amount, with nothing on the reading
/// saying so. Pairing is where that becomes visible data, so it is refused until the question is
/// answered rather than guarded further downstream.
#[must_use]
pub fn pairing_refusal(source_system: &str) -> Option<String> {
    (source_system.eq_ignore_ascii_case("nomis")).then(|| {
        "NOMIS streams cannot be paired yet: the portal reports a date and a time with no zone, \
         and whether they are UTC or Valais wall clock is unconfirmed (ADR 0004). Pairing one \
         would attribute every grab to a timestamp that may be one or two hours off."
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::pairing_refusal;

    #[test]
    fn nomis_is_refused_and_says_why() {
        let reason = pairing_refusal("nomis").expect("NOMIS is refused");
        assert!(reason.contains("no zone"), "{reason}");
        assert!(reason.contains("ADR 0004"), "{reason}");
        assert!(
            pairing_refusal("NOMIS").is_some(),
            "the source system is compared without regard to case"
        );
    }

    #[test]
    fn every_other_source_pairs() {
        for source in ["cnet", "metalp", "vaisala", "api", "grab_sample"] {
            assert!(pairing_refusal(source).is_none(), "{source} pairs");
        }
    }
}
