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
    Alias, DeleteStatement, Expr, Func, InsertStatement, PostgresQueryBuilder, Query,
    SelectStatement, UpdateStatement,
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, FromQueryResult, Statement, TransactionSession,
    TransactionTrait,
};

use crate::error::{AppError, AppResult};

const LIFT_CAP: &str = "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0";

/// The span row the pre-write query reports.
#[derive(FromQueryResult)]
struct TouchedSpan {
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
    /// The statement as SQL, for a caller checking what it built.
    fn to_sql(&self) -> String {
        match self {
            Self::Update(statement) => statement.to_string(PostgresQueryBuilder),
            Self::Delete(statement) => statement.to_string(PostgresQueryBuilder),
            Self::Insert(statement) => statement.to_string(PostgresQueryBuilder),
        }
    }

    /// The statement as built, with no `RETURNING`.
    fn build(self) -> Statement {
        let (sql, values) = match self {
            Self::Update(statement) => statement.build(PostgresQueryBuilder),
            Self::Delete(statement) => statement.build(PostgresQueryBuilder),
            Self::Insert(statement) => statement.build(PostgresQueryBuilder),
        };
        Statement::from_sql_and_values(DatabaseBackend::Postgres, sql, values)
    }

}

/// A hypertable write and the query naming the rows it is about to touch, from the same table on
/// the same predicate, selecting their `time`.
///
/// The span is read from that query before the statement runs. TimescaleDB holds every row a
/// hypertable `RETURNING` emits in the statement's executor memory, so a write that can reach a
/// stream's whole history cannot report its span that way: the attribution UPDATE over 285k rows
/// peaks at 25 MB without it and 1.48 GB with it.
pub struct Spanned {
    rows: SelectStatement,
    write: Dml,
}

impl Spanned {
    /// `rows` selects `time` over the rows `write` changes. The two carry one predicate between
    /// them, so a caller builds it once and hands it to both. The span is read before the write,
    /// so an `INSERT` has none to read and takes [`mutation_rows`] instead.
    pub fn new(rows: SelectStatement, write: impl Into<Dml>) -> Self {
        Self {
            rows,
            write: write.into(),
        }
    }

    /// The write and the query of the rows it touches, as SQL. The pair is only correct while the
    /// query selects the rows the write changes, so it is rendered to be compared.
    #[must_use]
    pub fn as_sql(&self) -> (String, String) {
        (
            self.write.to_sql(),
            self.rows.to_string(PostgresQueryBuilder),
        )
    }
}

/// One hypertable DML statement in its own guarded transaction, reporting the rows and the time span
/// it touched.
pub async fn guarded_mutation<C: TransactionTrait>(
    db: &C,
    spanned: Spanned,
) -> AppResult<TouchedRange> {
    guarded(db, async |txn| mutation(txn, spanned).await).await
}

/// One hypertable DML statement on a connection that is already inside a guarded transaction,
/// reporting the rows and the time span it touched. The span is read first, in the same
/// transaction, so it names the rows as the statement is about to find them.
pub async fn mutation<C: ConnectionTrait>(conn: &C, spanned: Spanned) -> AppResult<TouchedRange> {
    let Spanned { rows, write } = spanned;
    let span = span_of(conn, rows).await?;
    let rows_written = conn.execute_raw(write.build()).await?.rows_affected();
    Ok(TouchedRange {
        rows: rows_written,
        min_time: span.min_time,
        max_time: span.max_time,
    })
}

/// `MIN(time)` and `MAX(time)` over the caller's query of the rows, as a subquery.
fn span_query(rows: SelectStatement) -> SelectStatement {
    let time = Alias::new("time");
    let touched = Alias::new("touched");
    Query::select()
        .expr_as(
            Func::min(Expr::col((touched.clone(), time.clone()))),
            Alias::new("min_time"),
        )
        .expr_as(
            Func::max(Expr::col((touched.clone(), time))),
            Alias::new("max_time"),
        )
        .from_subquery(rows, touched)
        .to_owned()
}

/// The `[min, max]` of `time` over the rows a write is about to touch.
async fn span_of<C: ConnectionTrait>(conn: &C, rows: SelectStatement) -> AppResult<TouchedSpan> {
    let (sql, values) = span_query(rows).build(PostgresQueryBuilder);
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .ok_or_else(|| AppError::Internal("Guarded mutation read no span row".to_string()))?;
    Ok(TouchedSpan::from_query_result(&row, "")?)
}

/// One hypertable DML statement on a connection already inside a guarded transaction, reporting
/// only the rows it wrote. TimescaleDB holds every row a hypertable `RETURNING` emits in the
/// statement's executor memory, so a write that can reach a stream's whole history and needs no
/// span takes this rather than [`mutation`].
pub async fn mutation_rows<C: ConnectionTrait>(
    conn: &C,
    statement: impl Into<Dml>,
) -> AppResult<u64> {
    Ok(conn
        .execute_raw(statement.into().build())
        .await?
        .rows_affected())
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
