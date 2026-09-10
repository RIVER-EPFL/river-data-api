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
use sea_orm::sea_query::{
    Alias, CommonTableExpression, DeleteStatement, Expr, Func, InsertStatement,
    PostgresQueryBuilder, Query, SubQueryStatement, UpdateStatement, WithClause,
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, FromQueryResult, Statement, TransactionSession,
    TransactionTrait,
};

use crate::error::{AppError, AppResult};

const LIFT_CAP: &str = "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0";

/// The summary row a guarded statement reports, as the wrapper selects it.
#[derive(FromQueryResult)]
struct TouchedSummary {
    touched_rows: i64,
    min_time: Option<chrono::DateTime<chrono::Utc>>,
    max_time: Option<chrono::DateTime<chrono::Utc>>,
}

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
    conn.execute_raw(Statement::from_string(
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
    F: AsyncFnOnce(&C::Transaction) -> AppResult<T>,
{
    let txn = db.begin().await?;
    lift_decompression_cap(&txn).await?;
    crate::common::actor::declare(&txn).await?;
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

/// Run `work` in one transaction with the decompression cap lifted and roll it back either way,
/// so what the work did is read back from the write itself and then undone. This is how a preview
/// shows the arithmetic a commit would do without implementing it twice.
pub async fn guarded_rollback<C, F, T>(db: &C, work: F) -> AppResult<T>
where
    C: TransactionTrait,
    F: AsyncFnOnce(&C::Transaction) -> AppResult<T>,
{
    let txn = db.begin().await?;
    lift_decompression_cap(&txn).await?;
    crate::common::actor::declare(&txn).await?;
    let outcome = work(&txn).await;
    if let Err(rollback) = txn.rollback().await {
        tracing::warn!(error = %rollback, "Rollback after a preview failed");
    }
    outcome
}

/// The three statement kinds a guarded mutation accepts, so a caller hands over what the builder
/// produced rather than SQL text.
pub enum Dml {
    Update(UpdateStatement),
    Delete(DeleteStatement),
    Insert(InsertStatement),
}

impl From<UpdateStatement> for Dml {
    fn from(statement: UpdateStatement) -> Self {
        Self::Update(statement)
    }
}

impl From<DeleteStatement> for Dml {
    fn from(statement: DeleteStatement) -> Self {
        Self::Delete(statement)
    }
}

impl From<InsertStatement> for Dml {
    fn from(statement: InsertStatement) -> Self {
        Self::Insert(statement)
    }
}

impl Dml {
    /// The statement with `RETURNING time` on it, as the summary wrapper needs.
    fn returning_time(self) -> SubQueryStatement {
        let time = Alias::new("time");
        match self {
            Self::Update(mut statement) => {
                statement.returning_col(time);
                SubQueryStatement::UpdateStatement(statement)
            }
            Self::Delete(mut statement) => {
                statement.returning_col(time);
                SubQueryStatement::DeleteStatement(statement)
            }
            Self::Insert(mut statement) => {
                statement.returning_col(time);
                SubQueryStatement::InsertStatement(statement)
            }
        }
    }
}

/// One hypertable DML statement in its own guarded transaction, reporting the rows and the time span
/// it touched.
pub async fn guarded_mutation<C: TransactionTrait>(
    db: &C,
    statement: impl Into<Dml>,
) -> AppResult<TouchedRange> {
    let statement = summary_of(statement.into());
    guarded(db, async |txn| run_summary(txn, statement).await).await
}

/// One hypertable DML statement on a connection that is already inside a guarded transaction,
/// reporting the rows and the time span it touched. The statement must be an `UPDATE`, `INSERT` or
/// `DELETE` against a table with a `time` column, and must not carry its own `RETURNING`.
pub async fn mutation<C: ConnectionTrait>(
    conn: &C,
    statement: impl Into<Dml>,
) -> AppResult<TouchedRange> {
    run_summary(conn, summary_of(statement.into())).await
}

/// [`guarded_mutation`] over a statement still spelled as text.
pub async fn guarded_mutation_sql<C: TransactionTrait>(
    db: &C,
    statement: Statement,
) -> AppResult<TouchedRange> {
    guarded(db, async |txn| mutation_sql(txn, statement).await).await
}

/// The same wrapper over a statement still spelled as text. Every caller here is a lifted
/// hypertable write not yet expressed through the builder.
pub async fn mutation_sql<C: ConnectionTrait>(
    conn: &C,
    statement: Statement,
) -> AppResult<TouchedRange> {
    run_summary(
        conn,
        Statement {
            sql: wrap_returning_time(&statement.sql),
            values: statement.values,
            db_backend: statement.db_backend,
        },
    )
    .await
}

/// Read back the summary a wrapped statement returns. Takes the statement already wrapped, so
/// nothing has to recognise the wrapper by its own text.
async fn run_summary<C: ConnectionTrait>(
    conn: &C,
    statement: Statement,
) -> AppResult<TouchedRange> {
    let row = conn.query_one_raw(statement).await?.ok_or_else(|| {
        AppError::Internal("Guarded mutation returned no summary row".to_string())
    })?;
    let summary = TouchedSummary::from_query_result(&row, "")?;
    Ok(TouchedRange {
        rows: u64::try_from(summary.touched_rows).unwrap_or(0),
        min_time: summary.min_time,
        max_time: summary.max_time,
    })
}

/// The same wrapper as [`wrap_returning_time`], built rather than formatted.
fn summary_of(statement: Dml) -> Statement {
    let mut mutated = CommonTableExpression::new();
    mutated
        .table_name(Alias::new("mutated"))
        .query(statement.returning_time());
    let with = WithClause::new().cte(mutated).to_owned();

    let query = Query::select()
        .expr_as(Expr::cust("COUNT(*)::bigint"), Alias::new("touched_rows"))
        .expr_as(
            Func::min(Expr::col(Alias::new("time"))),
            Alias::new("min_time"),
        )
        .expr_as(
            Func::max(Expr::col(Alias::new("time"))),
            Alias::new("max_time"),
        )
        .from(Alias::new("mutated"))
        .to_owned()
        .with(with);

    let (sql, values) = query.build(PostgresQueryBuilder);
    Statement::from_sql_and_values(DatabaseBackend::Postgres, sql, values)
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
#[path = "tests/bulk_write_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/bulk_write_delete_sites.rs"]
mod delete_sites;

#[cfg(test)]
#[path = "tests/bulk_write_lift_sites.rs"]
mod lift_sites;
