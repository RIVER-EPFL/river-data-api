//! The one rule deciding when readings sharing an instant form a `samples` row, and the one SQL
//! materialiser every path that applies the rule after the readings are written goes through.
//!
//! The database triggers on `readings` answer a different question: given a `sample_id`, what are
//! that sample's statistics, and does any reading still point at it. They never create a `samples`
//! row, so the rule below is the only place a sample's existence begins. A sample nothing references
//! any more is reaped by the trigger, which is garbage collection rather than a judgement about what
//! a group is.

use sea_orm::{ConnectionTrait, Statement};

use crate::common::bulk_write;
use crate::error::AppResult;

/// Grabs are spot measurements: a bottle, not a logger cadence.
pub const SPOT: &str = "spot";

/// How many readings sharing a slot instant make a sample. A single measurement is a reading, not
/// a group of them: mean, min and max would be the value and sd undefined, so the row would only
/// denormalise what the reading already says.
pub const MIN_REPLICATES: usize = 2;

/// A `(site, parameter, instant)` group forms a `samples` row when it carries two or more spot
/// readings on a paired slot, whoever wrote them and whatever they declared.
///
/// A spot instant with no sample row is by definition a single measurement, which is why serving,
/// the visits grid and every export derive n = 1 from the reading rather than from a row here.
#[must_use]
pub const fn forms_sample(replicates: usize) -> bool {
    replicates >= MIN_REPLICATES
}

/// The groups a selection of readings forms, as SQL: unstamped spot readings on an attributed slot,
/// grouped by `(site, parameter, instant)`, kept when the group reaches [`MIN_REPLICATES`] or when
/// the instant already has a sample for a late replicate to join.
fn group_select_sql(row_predicate: &str) -> String {
    format!(
        "SELECT r.site_id, r.parameter_id, r.time
         FROM readings r
         JOIN data_streams ds ON r.stream_id = ds.id
         WHERE {row_predicate}
           AND r.sample_id IS NULL
           AND r.site_id IS NOT NULL
           AND r.parameter_id IS NOT NULL
           AND r.measurement_type = '{SPOT}'
         GROUP BY r.site_id, r.parameter_id, r.time
         HAVING COUNT(*) >= {MIN_REPLICATES}
             OR EXISTS (SELECT 1 FROM samples s2
                         WHERE s2.site_id = r.site_id
                           AND s2.parameter_id = r.parameter_id
                           AND s2.collected_at = r.time)"
    )
}

/// Find-or-create the `samples` rows for the groups a selection of readings forms, then stamp
/// `sample_id` onto the readings of those groups.
///
/// `row_predicate` is SQL over the aliases `r` (`readings`) and `ds` (`data_streams`), taking the
/// bind values given, eg. `r.stream_id = $1`. It selects which readings are in scope and is applied
/// to the grouping and to the stamping alike, so the stamping cannot reach an unrelated stream's
/// reading that happens to sit on the same slot at the same instant. Grouping is always by
/// `(site_id, parameter_id, time)`, which is the `samples` unique key, so the find-or-create and
/// the stamping cannot disagree about what a group is.
///
/// A group whose sample already exists takes the late replicate whatever the unstamped count is:
/// the rule is about how many readings share the instant, not how many arrived in this write.
pub async fn materialise_samples<C: ConnectionTrait>(
    conn: &C,
    row_predicate: &str,
    binds: Vec<sea_orm::Value>,
) -> AppResult<()> {
    materialise_samples_with_estimator(conn, row_predicate, binds, None).await
}

/// [`materialise_samples`] for a caller that knows the stream's declared sd estimator.
///
/// The estimator is resolved per group rather than per call: one predicate can span several slots,
/// and each carries its own declaration. `stream_spec` is the stream's own declaration, which wins
/// over the slot's; absent it, the slot decides, and absent that the group is recorded undeclared.
pub async fn materialise_samples_with_estimator<C: ConnectionTrait>(
    conn: &C,
    row_predicate: &str,
    binds: Vec<sea_orm::Value>,
    stream_spec: Option<&str>,
) -> AppResult<()> {
    let group_select = group_select_sql(row_predicate);

    // The estimator each new row is computed with, and what chose it, decided in the insert so a
    // group can never exist without both recorded. A stream declaration outranks the slot's; with
    // neither, the row is stamped `default`, which is the undeclared state the report lists and
    // the audit gate reads. The stream's value is a stored spec field, so it goes through
    // `sd_estimator::parse` and reaches the SQL as one of two literals, never as caller text.
    let declared_by_stream = super::sd_estimator::parse_opt(stream_spec)?;
    let estimator_sql = match declared_by_stream {
        Some(declared) => format!("'{declared}', 'stream'"),
        None => "COALESCE(sp.sd_estimator, 'sample'), \
                 CASE WHEN sp.sd_estimator IS NULL THEN 'default' ELSE 'slot' END"
            .to_string(),
    };
    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "INSERT INTO samples (site_id, parameter_id, collected_at,
                                  sd_estimator, sd_estimator_source)
             SELECT g.site_id, g.parameter_id, g.time, {estimator_sql}
             FROM ({group_select}) g
             LEFT JOIN site_parameters sp
               ON sp.site_id = g.site_id AND sp.parameter_id = g.parameter_id
             ON CONFLICT (site_id, parameter_id, collected_at) DO NOTHING"
        ),
        binds.clone(),
    ))
    .await?;

    // The stamping UPDATE can reach chunks the compression policy already closed.
    bulk_write::lift_decompression_cap(conn).await?;
    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "UPDATE readings r
             SET sample_id = s.id
             FROM data_streams ds, ({group_select}) g
             JOIN samples s
               ON s.site_id = g.site_id
              AND s.parameter_id = g.parameter_id
              AND s.collected_at = g.time
             WHERE r.stream_id = ds.id
               AND {row_predicate}
               AND r.site_id = g.site_id
               AND r.parameter_id = g.parameter_id
               AND r.time = g.time
               AND r.sample_id IS NULL
               AND r.measurement_type = '{SPOT}'"
        ),
        binds,
    ))
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MIN_REPLICATES, forms_sample, group_select_sql};

    #[test]
    fn a_group_is_two_or_more_readings() {
        assert!(!forms_sample(0), "an empty group is not a sample");
        assert!(!forms_sample(1), "a single measurement is the reading itself");
        assert!(forms_sample(2), "two readings at one instant are a group");
        assert!(forms_sample(3));
        assert_eq!(MIN_REPLICATES, 2);
    }

    #[test]
    fn the_group_query_counts_readings_and_admits_a_late_replicate() {
        let sql = group_select_sql("r.stream_id = $1");
        assert!(
            sql.contains("HAVING COUNT(*) >= 2"),
            "the minimum is the one rule, in the query: {sql}"
        );
        assert!(
            sql.contains("EXISTS (SELECT 1 FROM samples s2"),
            "a group whose sample exists takes a late replicate of one: {sql}"
        );
        assert!(
            sql.contains("r.measurement_type = 'spot'"),
            "only spot instants have replicates: {sql}"
        );
        assert!(
            sql.contains("r.sample_id IS NULL"),
            "already-stamped readings are not regrouped: {sql}"
        );
        assert!(sql.contains("r.stream_id = $1"), "the caller's scope applies: {sql}");
    }
}
