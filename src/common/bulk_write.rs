//! Guarded bulk writes against the `readings` and `status_events` hypertables.
//!
//! Any bulk DML on those tables can reach chunks the 30-day policy has compressed. TimescaleDB caps
//! how many tuples one statement may decompress
//! (`timescaledb.max_tuples_decompressed_per_dml_transaction`, default 100k) and the cap can only be
//! lifted with `SET LOCAL`, which needs a transaction. The transaction is therefore both the
//! atomicity and the only scope in which the lift exists: every bulk write goes through [`guarded`]
//! (or one of the single-statement wrappers) so neither can be forgotten.
//!
//! A continuous-aggregate refresh is a procedure and cannot run inside a transaction block. Run it
//! after the guarded call returns, on the [`TouchedRange`] the write reports:
//!
//! ```text
//! let touched = bulk_write::guarded_mutation(&state.db, stmt).await?;
//! if let Some(window) = aggregates::Window::touched(&touched) {
//!     aggregates::refresh(&state.db, window).await?;
//! }
//! ```

use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseTransaction, Statement, TransactionTrait};

use crate::error::{AppError, AppResult};

const LIFT_CAP: &str = "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0";

/// Rows written by a guarded statement and the span of `time` they cover.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TouchedRange {
    pub rows: u64,
    pub min_time: Option<DateTime<Utc>>,
    pub max_time: Option<DateTime<Utc>>,
}

impl TouchedRange {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// The `[min, max]` span, or `None` when the statement matched nothing.
    #[must_use]
    pub fn span(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.min_time.zip(self.max_time)
    }

    /// Everything two statements touched between them, for a guarded transaction running several
    /// (a chunk loop, or readings plus status_events) that refreshes once at the end.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        let pick =
            |a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>, keep_earlier: bool| match (a, b) {
                (Some(a), Some(b)) => Some(if keep_earlier { a.min(b) } else { a.max(b) }),
                (some, None) | (None, some) => some,
            };
        Self {
            rows: self.rows + other.rows,
            min_time: pick(self.min_time, other.min_time, true),
            max_time: pick(self.max_time, other.max_time, false),
        }
    }
}

/// Lift the decompression cap on a transaction the caller already owns. Prefer [`guarded`], which
/// cannot be called without it; this exists for a transaction opened for other reasons that also
/// carries hypertable DML.
pub async fn lift_decompression_cap<C: ConnectionTrait>(conn: &C) -> AppResult<()> {
    conn.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        LIFT_CAP.to_owned(),
    ))
    .await?;
    Ok(())
}

/// Run `work` in one transaction with the decompression cap lifted, committing on `Ok` and rolling
/// back on `Err`. Every bulk write to `readings` / `status_events` belongs in here.
///
/// `db` may itself be a transaction, in which case the work runs in a savepoint and the lift applies
/// to the enclosing transaction.
pub async fn guarded<C, F, T>(db: &C, work: F) -> AppResult<T>
where
    C: TransactionTrait,
    F: AsyncFnOnce(&DatabaseTransaction) -> AppResult<T>,
{
    let txn = db.begin().await?;
    lift_decompression_cap(&txn).await?;
    match work(&txn).await {
        Ok(value) => {
            txn.commit().await?;
            Ok(value)
        }
        Err(e) => {
            if let Err(rollback) = txn.rollback().await {
                tracing::warn!(error = %rollback, "Rollback after a failed guarded bulk write failed");
            }
            Err(e)
        }
    }
}

/// One hypertable DML statement in its own guarded transaction, reporting the rows and the time span
/// it touched.
pub async fn guarded_mutation<C: TransactionTrait>(
    db: &C,
    statement: Statement,
) -> AppResult<TouchedRange> {
    guarded(db, async |txn| mutation(txn, statement).await).await
}

/// One hypertable DML statement on a connection that is already inside a guarded transaction,
/// reporting the rows and the time span it touched. The statement must be an `UPDATE`, `INSERT` or
/// `DELETE` against a table with a `time` column, and must not carry its own `RETURNING`.
pub async fn mutation<C: ConnectionTrait>(
    conn: &C,
    statement: Statement,
) -> AppResult<TouchedRange> {
    let wrapped = Statement {
        sql: wrap_returning_time(&statement.sql),
        values: statement.values,
        db_backend: statement.db_backend,
    };
    let row = conn.query_one(wrapped).await?.ok_or_else(|| {
        AppError::Internal("Guarded mutation returned no summary row".to_string())
    })?;
    let rows: i64 = row.try_get("", "touched_rows")?;
    Ok(TouchedRange {
        rows: u64::try_from(rows).unwrap_or(0),
        min_time: row.try_get("", "min_time")?,
        max_time: row.try_get("", "max_time")?,
    })
}

/// Wrap a DML statement so it reports its row count and time span in one round trip.
fn wrap_returning_time(sql: &str) -> String {
    let body = sql.trim().trim_end_matches(';').trim_end();
    format!(
        "WITH mutated AS ({body} RETURNING time) \
         SELECT COUNT(*)::bigint AS touched_rows, MIN(time) AS min_time, MAX(time) AS max_time \
         FROM mutated"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_returning_time_wraps_an_update() {
        let wrapped =
            wrap_returning_time("UPDATE readings SET site_id = NULL WHERE stream_id = $1");
        assert!(wrapped.starts_with(
            "WITH mutated AS (UPDATE readings SET site_id = NULL WHERE stream_id = $1 RETURNING time)"
        ));
        assert!(wrapped.contains("COUNT(*)::bigint AS touched_rows"));
        assert!(wrapped.contains("MIN(time) AS min_time"));
        assert!(wrapped.contains("MAX(time) AS max_time"));
    }

    #[test]
    fn test_wrap_returning_time_strips_a_trailing_semicolon() {
        let wrapped = wrap_returning_time("DELETE FROM readings WHERE stream_id = $1 ;\n");
        assert!(wrapped.contains("WHERE stream_id = $1 RETURNING time)"));
        assert!(!wrapped.contains(';'));
    }

    #[test]
    fn test_touched_range_span_needs_both_bounds() {
        let empty = TouchedRange::default();
        assert!(empty.is_empty());
        assert!(empty.span().is_none());

        let t = DateTime::parse_from_rfc3339("2026-08-12T14:22:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let one = TouchedRange {
            rows: 1,
            min_time: Some(t),
            max_time: Some(t),
        };
        assert!(!one.is_empty());
        assert_eq!(one.span(), Some((t, t)));
    }

    #[test]
    fn test_merge_widens_the_span_and_sums_the_rows() {
        let early = DateTime::parse_from_rfc3339("2026-08-12T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let late = DateTime::parse_from_rfc3339("2026-08-12T18:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let a = TouchedRange {
            rows: 2,
            min_time: Some(late),
            max_time: Some(late),
        };
        let b = TouchedRange {
            rows: 3,
            min_time: Some(early),
            max_time: Some(early),
        };
        let merged = a.merge(b);
        assert_eq!(merged.rows, 5);
        assert_eq!(merged.span(), Some((early, late)));
    }

    #[test]
    fn test_merge_with_an_empty_range_keeps_the_other_span() {
        let t = DateTime::parse_from_rfc3339("2026-08-12T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let one = TouchedRange {
            rows: 1,
            min_time: Some(t),
            max_time: Some(t),
        };
        assert_eq!(one.merge(TouchedRange::default()), one);
        assert_eq!(TouchedRange::default().merge(one), one);
        assert_eq!(
            TouchedRange::default().merge(TouchedRange::default()),
            TouchedRange::default()
        );
    }
}
