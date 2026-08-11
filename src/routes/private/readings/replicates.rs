use sea_orm::{ConnectionTrait, DbErr, Statement};
use uuid::Uuid;

/// Renumber replicate groups to 0..n-1 for the given streams. Sources that label replicates by
/// position (NOMIS A-D, portal column suffixes) can leave a group without an index-0 row, and every
/// serving path reads index 0, so those readings would be invisible.
///
/// Runs in two passes because `replicate_index` is part of the readings primary key: the affected
/// rows are parked above the offset first, then written back densely.
pub async fn densify_stream_replicates<C: ConnectionTrait>(
    db: &C,
    stream_ids: &[Uuid],
) -> Result<u64, DbErr> {
    if stream_ids.is_empty() {
        return Ok(0);
    }
    // replicate_index is a smallint; the offset only has to clear any real group size.
    const PARK_OFFSET: i32 = 1_000;

    let parked = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings r
              SET replicate_index = r.replicate_index + $2
              FROM (
                  SELECT stream_id, time FROM readings
                  WHERE stream_id = ANY($1)
                  GROUP BY stream_id, time
                  HAVING MIN(replicate_index) > 0 AND MAX(replicate_index) < $2
              ) g
              WHERE r.stream_id = g.stream_id AND r.time = g.time",
            [stream_ids.to_vec().into(), PARK_OFFSET.into()],
        ))
        .await?
        .rows_affected();
    if parked == 0 {
        return Ok(0);
    }

    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE readings r
          SET replicate_index = d.dense_index
          FROM (
              SELECT stream_id, time, replicate_index,
                     (row_number() OVER (PARTITION BY stream_id, time ORDER BY replicate_index) - 1)::int
                         AS dense_index
              FROM readings
              WHERE stream_id = ANY($1) AND replicate_index >= $2
          ) d
          WHERE r.stream_id = d.stream_id AND r.time = d.time
            AND r.replicate_index = d.replicate_index",
        [stream_ids.to_vec().into(), PARK_OFFSET.into()],
    ))
    .await?;

    Ok(parked)
}
